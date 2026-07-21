use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, NaiveDate, Weekday};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Sparkline, Wrap},
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};
use tokscale_core::{generate_graph_result, ClientContribution, DailyContribution, DailyTotals};

#[derive(Debug, Parser)]
#[command(
    name = "tokscale-export-merge",
    about = "Merge tokscale JSON exports from separate machines and view the combined contribution graph"
)]
struct Args {
    /// Export files. If omitted, uses tokscale-export-*.json in the current directory.
    #[arg(value_name = "EXPORT.json")]
    files: Vec<PathBuf>,

    /// Where to write the merged export.
    #[arg(short, long, default_value = "tokscale-merged.json")]
    output: PathBuf,

    /// Only write the merged JSON; do not start the TUI.
    #[arg(long)]
    no_tui: bool,

    /// Override the machine label for an input. Repeat as NAME=EXPORT.json.
    #[arg(long = "machine", value_name = "NAME=EXPORT.json")]
    machine_files: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    cache_read: u64,
    #[serde(default)]
    cache_write: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    reasoning: u64,
    #[serde(default)]
    total: u64,
}

impl Tokens {
    fn component_total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.reasoning)
    }

    /// Older exports omit reasoning even though `total` may include it.
    fn effective_total(&self) -> u64 {
        if self.total == 0 {
            self.component_total()
        } else {
            self.total
        }
    }

    fn add_assign(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.total = self.total.saturating_add(other.effective_total());
    }

    fn to_core(&self) -> tokscale_core::TokenBreakdown {
        let base = self.component_total();
        let implicit_reasoning = self.effective_total().saturating_sub(base);
        tokscale_core::TokenBreakdown {
            input: self.input.min(i64::MAX as u64) as i64,
            output: self.output.min(i64::MAX as u64) as i64,
            cache_read: self.cache_read.min(i64::MAX as u64) as i64,
            cache_write: self.cache_write.min(i64::MAX as u64) as i64,
            reasoning: self
                .reasoning
                .saturating_add(implicit_reasoning)
                .min(i64::MAX as u64) as i64,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Performance {
    #[serde(rename = "msPer1KTokens", default)]
    ms_per_1k_tokens: Option<f64>,
    #[serde(default)]
    sample_count: u64,
    #[serde(default)]
    timed_tokens: u64,
    #[serde(default)]
    token_coverage: f64,
    #[serde(default)]
    total_duration_ms: u64,
}

impl Performance {
    fn add_assign(&mut self, other: &Self) {
        self.sample_count = self.sample_count.saturating_add(other.sample_count);
        self.timed_tokens = self.timed_tokens.saturating_add(other.timed_tokens);
        self.total_duration_ms = self
            .total_duration_ms
            .saturating_add(other.total_duration_ms);
    }

    fn finalize(&mut self, total_tokens: u64) {
        self.ms_per_1k_tokens = if self.timed_tokens > 0 && self.total_duration_ms > 0 {
            Some(self.total_duration_ms as f64 * 1000.0 / self.timed_tokens as f64)
        } else {
            None
        };
        self.token_coverage = if total_tokens > 0 {
            (self.timed_tokens as f64 / total_tokens as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelExport {
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    client: String,
    #[serde(default)]
    tokens: Tokens,
    #[serde(default)]
    cost: f64,
    #[serde(default)]
    performance: Performance,
    #[serde(default)]
    session_count: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentExport {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    clients: String,
    #[serde(default)]
    tokens: Tokens,
    #[serde(default)]
    cost: f64,
    #[serde(default)]
    message_count: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyExport {
    date: String,
    #[serde(default)]
    tokens: Tokens,
    #[serde(default)]
    message_count: u64,
    #[serde(default)]
    turn_count: u64,
    #[serde(default)]
    cost: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MonthlyExport {
    month: String,
    #[serde(default)]
    tokens: Tokens,
    #[serde(default)]
    message_count: u64,
    #[serde(default)]
    turn_count: u64,
    #[serde(default)]
    cost: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ExportFile {
    #[serde(default)]
    models: Vec<ModelExport>,
    #[serde(default)]
    agents: Vec<AgentExport>,
    #[serde(default)]
    daily: Vec<DailyExport>,
    #[serde(skip)]
    machine_label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct MachineUsage {
    machine: String,
    source_file: String,
    tokens: Tokens,
    cost: f64,
    model_count: usize,
    agent_count: usize,
    session_count: u64,
    message_count: u64,
    turn_count: u64,
    active_days: usize,
}

#[derive(Debug, Clone)]
struct MergedExport {
    models: Vec<ModelExport>,
    agents: Vec<AgentExport>,
    machines: Vec<MachineUsage>,
    daily: Vec<DailyExport>,
    monthly: Vec<MonthlyExport>,
    total_tokens: u64,
    total_cost: f64,
    source_count: usize,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn clean_cost(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}

fn labels(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn join_labels(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn union_labels(left: &str, right: &str) -> String {
    let mut merged = labels(left);
    merged.extend(labels(right));
    join_labels(&merged)
}

fn canonical_model(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        "unknown".to_string()
    } else {
        tokscale_core::canonical_model_id(model)
    }
}

fn read_files(entries: &[(Option<String>, PathBuf)]) -> Result<Vec<(PathBuf, ExportFile)>> {
    entries
        .iter()
        .map(|(machine_label, path)| {
            let text = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut export: ExportFile = serde_json::from_str(&text)
                .with_context(|| format!("invalid tokscale export JSON: {}", path.display()))?;
            export.machine_label = machine_label.clone();
            Ok((path.clone(), export))
        })
        .collect()
}

fn parse_machine_spec(spec: &str) -> Result<(String, PathBuf)> {
    let (label, path) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("machine spec must be NAME=EXPORT.json: {spec}"))?;
    if label.trim().is_empty() || path.trim().is_empty() {
        bail!("machine spec must contain both NAME and EXPORT.json: {spec}");
    }
    Ok((label.trim().to_string(), PathBuf::from(path.trim())))
}

fn discover_files() -> Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(".")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("tokscale-export-") && name.ends_with(".json"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        bail!("no tokscale-export-*.json files found; pass export paths explicitly")
    }
    Ok(paths)
}

fn machine_usage(path: &Path, export: &ExportFile) -> MachineUsage {
    let machine = export.machine_label.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown-machine")
            .to_string()
    });
    let source_file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown.json")
        .to_string();

    let mut tokens = Tokens::default();
    let mut cost = 0.0;
    let mut session_count: u64 = 0;
    if export.models.is_empty() {
        for day in &export.daily {
            tokens.add_assign(&day.tokens);
            cost += clean_cost(day.cost);
        }
    } else {
        for model in &export.models {
            tokens.add_assign(&model.tokens);
            cost += clean_cost(model.cost);
            session_count = session_count.saturating_add(model.session_count);
        }
    }
    tokens.total = tokens.effective_total();

    let mut message_count: u64 = 0;
    let mut turn_count: u64 = 0;
    let mut active_days: usize = 0;
    for day in &export.daily {
        message_count = message_count.saturating_add(day.message_count);
        turn_count = turn_count.saturating_add(day.turn_count);
        if day.tokens.effective_total() > 0 || day.cost > 0.0 || day.message_count > 0 {
            active_days += 1;
        }
    }

    MachineUsage {
        machine,
        source_file,
        tokens,
        cost,
        model_count: export.models.len(),
        agent_count: export.agents.len(),
        session_count,
        message_count,
        turn_count,
        active_days,
    }
}

fn merge_exports(files: &[(PathBuf, ExportFile)]) -> MergedExport {
    let mut machines = files
        .iter()
        .map(|(path, export)| machine_usage(path, export))
        .collect::<Vec<_>>();
    machines.sort_by(|a, b| {
        clean_cost(b.cost)
            .total_cmp(&clean_cost(a.cost))
            .then_with(|| a.machine.cmp(&b.machine))
    });

    let mut models: BTreeMap<String, ModelExport> = BTreeMap::new();
    let mut agents: BTreeMap<String, AgentExport> = BTreeMap::new();
    let mut daily: BTreeMap<String, DailyExport> = BTreeMap::new();

    for (_, export) in files {
        for model in &export.models {
            // Grouping by canonical model intentionally folds the same model
            // across clients/providers, matching the TUI's model view.
            let key = canonical_model(&model.model);
            let entry = models.entry(key.clone()).or_insert_with(|| ModelExport {
                model: key.clone(),
                provider: String::new(),
                client: String::new(),
                ..ModelExport::default()
            });
            entry.model = key;
            entry.provider = union_labels(&entry.provider, &model.provider);
            entry.client = union_labels(&entry.client, &model.client);
            entry.tokens.add_assign(&model.tokens);
            entry.cost += clean_cost(model.cost);
            entry.performance.add_assign(&model.performance);
            entry.session_count = entry.session_count.saturating_add(model.session_count);
        }

        for agent in &export.agents {
            let display = if agent.agent.trim().is_empty() {
                "Unknown agent".to_string()
            } else {
                agent.agent.trim().to_string()
            };
            let key = display.to_lowercase();
            let entry = agents.entry(key).or_insert_with(|| AgentExport {
                agent: display.clone(),
                ..AgentExport::default()
            });
            entry.clients = union_labels(&entry.clients, &agent.clients);
            entry.tokens.add_assign(&agent.tokens);
            entry.cost += clean_cost(agent.cost);
            entry.message_count = entry.message_count.saturating_add(agent.message_count);
        }

        for day in &export.daily {
            let entry = daily
                .entry(day.date.clone())
                .or_insert_with(|| DailyExport {
                    date: day.date.clone(),
                    ..DailyExport::default()
                });
            entry.tokens.add_assign(&day.tokens);
            entry.message_count = entry.message_count.saturating_add(day.message_count);
            entry.turn_count = entry.turn_count.saturating_add(day.turn_count);
            entry.cost += clean_cost(day.cost);
        }
    }

    let mut models = models.into_values().collect::<Vec<_>>();
    for model in &mut models {
        model.tokens.total = model.tokens.effective_total();
        model.performance.finalize(model.tokens.total);
    }
    models.sort_by(|a, b| {
        clean_cost(b.cost)
            .total_cmp(&clean_cost(a.cost))
            .then_with(|| a.model.cmp(&b.model))
    });

    let mut agents = agents.into_values().collect::<Vec<_>>();
    for agent in &mut agents {
        agent.tokens.total = agent.tokens.effective_total();
    }
    agents.sort_by(|a, b| {
        clean_cost(b.cost)
            .total_cmp(&clean_cost(a.cost))
            .then_with(|| a.agent.cmp(&b.agent))
    });

    let mut daily = daily.into_values().collect::<Vec<_>>();
    for day in &mut daily {
        day.tokens.total = day.tokens.effective_total();
    }
    daily.sort_by(|a, b| b.date.cmp(&a.date));

    let mut monthly_map: BTreeMap<String, MonthlyExport> = BTreeMap::new();
    for day in &daily {
        if day.date.len() < 7 {
            continue;
        }
        let month = day.date[..7].to_string();
        let entry = monthly_map
            .entry(month.clone())
            .or_insert_with(|| MonthlyExport {
                month,
                ..MonthlyExport::default()
            });
        entry.tokens.add_assign(&day.tokens);
        entry.message_count = entry.message_count.saturating_add(day.message_count);
        entry.turn_count = entry.turn_count.saturating_add(day.turn_count);
        entry.cost += clean_cost(day.cost);
    }
    let mut monthly = monthly_map.into_values().collect::<Vec<_>>();
    for month in &mut monthly {
        month.tokens.total = month.tokens.effective_total();
    }
    monthly.sort_by(|a, b| b.month.cmp(&a.month));

    let total_tokens = models
        .iter()
        .map(|model| model.tokens.effective_total())
        .sum::<u64>();
    let total_cost = models
        .iter()
        .map(|model| clean_cost(model.cost))
        .sum::<f64>();

    MergedExport {
        models,
        agents,
        machines,
        daily,
        monthly,
        total_tokens,
        total_cost,
        source_count: files.len(),
    }
}

fn write_merged_export(path: &Path, merged: &MergedExport) -> Result<()> {
    let json = serde_json::json!({
        "models": merged.models,
        "agents": merged.agents,
        "machines": merged.machines,
        "daily": merged.daily,
        "monthly": merged.monthly,
        "totals": {
            "tokens": merged.total_tokens,
            "cost": merged.total_cost,
        }
    });
    let text = serde_json::to_string_pretty(&json)?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn to_core_contributions(daily: &[DailyExport]) -> Vec<DailyContribution> {
    let mut contributions = daily
        .iter()
        .filter_map(|day| {
            NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok()?;
            let tokens = day.tokens.to_core();
            Some(DailyContribution {
                date: day.date.clone(),
                totals: DailyTotals {
                    tokens: tokens.total(),
                    cost: clean_cost(day.cost),
                    messages: day.message_count.min(i32::MAX as u64) as i32,
                },
                intensity: 0,
                token_breakdown: tokens,
                clients: Vec::<ClientContribution>::new(),
                active_time_ms: None,
            })
        })
        .collect::<Vec<_>>();
    contributions.sort_by(|a, b| a.date.cmp(&b.date));
    contributions
}

#[derive(Debug, Clone)]
struct CalendarCell {
    intensity: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewTab {
    Overview,
    Daily,
    Monthly,
    Machines,
}

impl ViewTab {
    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Daily,
            Self::Daily => Self::Monthly,
            Self::Monthly => Self::Machines,
            Self::Machines => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Overview => Self::Machines,
            Self::Daily => Self::Overview,
            Self::Monthly => Self::Daily,
            Self::Machines => Self::Monthly,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Daily => "Daily",
            Self::Monthly => "Monthly",
            Self::Machines => "Machines",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PeriodTotals {
    tokens: Tokens,
    cost: f64,
    active_days: usize,
}

fn last_month_days(merged: &MergedExport) -> Vec<&DailyExport> {
    merged
        .daily
        .iter()
        .filter(|day| NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").is_ok())
        .take(30)
        .collect()
}

fn sum_daily(days: &[&DailyExport]) -> PeriodTotals {
    let mut totals = PeriodTotals::default();
    for day in days {
        totals.tokens.add_assign(&day.tokens);
        totals.cost += clean_cost(day.cost);
        if day.tokens.effective_total() > 0 || day.cost > 0.0 || day.message_count > 0 {
            totals.active_days += 1;
        }
    }
    totals.tokens.total = totals.tokens.effective_total();
    totals
}

fn sum_monthly(months: &[MonthlyExport]) -> PeriodTotals {
    let mut totals = PeriodTotals::default();
    for month in months {
        totals.tokens.add_assign(&month.tokens);
        totals.cost += clean_cost(month.cost);
        if month.tokens.effective_total() > 0 || month.cost > 0.0 || month.message_count > 0 {
            totals.active_days += 1;
        }
    }
    totals.tokens.total = totals.tokens.effective_total();
    totals
}

fn calendar_weeks(contributions: &[DailyContribution]) -> Vec<Vec<Option<CalendarCell>>> {
    let Some(last_date) = contributions
        .iter()
        .filter_map(|contribution| NaiveDate::parse_from_str(&contribution.date, "%Y-%m-%d").ok())
        .max()
    else {
        return Vec::new();
    };

    let days_from_sunday = match last_date.weekday() {
        Weekday::Sun => 0,
        Weekday::Mon => 1,
        Weekday::Tue => 2,
        Weekday::Wed => 3,
        Weekday::Thu => 4,
        Weekday::Fri => 5,
        Weekday::Sat => 6,
    };
    let start = last_date - ChronoDuration::days(364 + days_from_sunday);
    let map = contributions
        .iter()
        .filter_map(|contribution| {
            let date = NaiveDate::parse_from_str(&contribution.date, "%Y-%m-%d").ok()?;
            Some((date, contribution))
        })
        .collect::<HashMap<_, _>>();

    let mut weeks = Vec::new();
    let mut week = Vec::with_capacity(7);
    let mut date = start;
    while date <= last_date {
        week.push(map.get(&date).map(|contribution| CalendarCell {
            intensity: contribution.intensity,
        }));
        if date.weekday() == Weekday::Sat || date == last_date {
            weeks.push(week);
            week = Vec::with_capacity(7);
        }
        date += ChronoDuration::days(1);
    }
    weeks
}

fn format_tokens(tokens: u64) -> String {
    const UNITS: [&str; 4] = ["", "K", "M", "B"];
    let mut value = tokens as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        tokens.to_string()
    } else {
        format!("{value:.2}{}", UNITS[unit])
    }
}

fn compact_label(label: &str, max_chars: usize) -> String {
    let chars = label.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return label.to_string();
    }
    let suffix_len = max_chars.saturating_sub(1);
    let suffix = chars[chars.len().saturating_sub(suffix_len)..]
        .iter()
        .collect::<String>();
    format!("…{suffix}")
}

fn heat_color(intensity: u8) -> Color {
    match intensity {
        0 => Color::Rgb(35, 40, 48),
        1 => Color::Rgb(43, 78, 55),
        2 => Color::Rgb(52, 118, 70),
        3 => Color::Rgb(72, 166, 86),
        _ => Color::Rgb(121, 214, 112),
    }
}

fn draw_tui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    merged: &MergedExport,
) -> Result<()> {
    let contributions = to_core_contributions(&merged.daily);
    let graph = generate_graph_result(contributions.clone(), 0);
    let weeks = calendar_weeks(&graph.contributions);
    let mut tab = ViewTab::Overview;

    loop {
        terminal.draw(|frame| render(frame, merged, &graph, &weeks, tab))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(KeyEvent { code, .. }) = event::read()? {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('1') => tab = ViewTab::Overview,
                    KeyCode::Char('2') => tab = ViewTab::Daily,
                    KeyCode::Char('3') => tab = ViewTab::Monthly,
                    KeyCode::Char('4') => tab = ViewTab::Machines,
                    KeyCode::Tab | KeyCode::Right => tab = tab.next(),
                    KeyCode::BackTab | KeyCode::Left => tab = tab.previous(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn render(
    frame: &mut Frame<'_>,
    merged: &MergedExport,
    graph: &tokscale_core::GraphResult,
    weeks: &[Vec<Option<CalendarCell>>],
    tab: ViewTab,
) {
    let root = frame.area();
    let outer = Block::default()
        .title(format!(" TOKSCALE // MULTI-MACHINE · {} ", tab.label()))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(outer, root);
    let inner = Block::default().borders(Borders::ALL).inner(root);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(inner);

    render_nav(frame, rows[0], tab);
    match tab {
        ViewTab::Overview => {
            let all_tokens = Tokens {
                total: merged.total_tokens,
                ..Tokens::default()
            };
            render_stats(
                frame,
                rows[1],
                StatsView {
                    period: "ALL-TIME",
                    tokens: &all_tokens,
                    cost: merged.total_cost,
                    active_days: graph.summary.active_days as usize,
                    detail_label: "MODELS",
                    detail_value: merged.models.len().to_string(),
                },
            );
            let middle = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
                .split(rows[2]);
            render_heatmap(frame, middle[0], weeks, graph);
            render_models(frame, middle[1], merged);
            render_recent_days(frame, rows[3], merged);
        }
        ViewTab::Daily => {
            let days = last_month_days(merged);
            let totals = sum_daily(&days);
            render_stats(
                frame,
                rows[1],
                StatsView {
                    period: "LAST 30 DAYS",
                    tokens: &totals.tokens,
                    cost: totals.cost,
                    active_days: totals.active_days,
                    detail_label: "AVG/DAY",
                    detail_value: format!("${:.2}", totals.cost / totals.active_days.max(1) as f64),
                },
            );
            render_daily_tab(frame, rows[2], rows[3], &days, &totals, merged);
        }
        ViewTab::Monthly => {
            let totals = sum_monthly(&merged.monthly);
            render_stats(
                frame,
                rows[1],
                StatsView {
                    period: "ALL MONTHS",
                    tokens: &totals.tokens,
                    cost: totals.cost,
                    active_days: totals.active_days,
                    detail_label: "MONTHS",
                    detail_value: merged.monthly.len().to_string(),
                },
            );
            render_monthly_tab(frame, rows[2], rows[3], &merged.monthly, &totals, merged);
        }
        ViewTab::Machines => {
            let all_tokens = Tokens {
                total: merged.total_tokens,
                ..Tokens::default()
            };
            render_stats(
                frame,
                rows[1],
                StatsView {
                    period: "ALL MACHINES",
                    tokens: &all_tokens,
                    cost: merged.total_cost,
                    active_days: graph.summary.active_days as usize,
                    detail_label: "MACHINES",
                    detail_value: merged.machines.len().to_string(),
                },
            );
            render_machine_tab(frame, rows[2], rows[3], merged);
        }
    }

    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " q ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("quit  "),
        Span::styled("•", Style::default().fg(Color::DarkGray)),
        Span::raw("  1 overview  2 daily  3 monthly  4 machines  "),
        Span::styled("•", Style::default().fg(Color::DarkGray)),
        Span::raw("  arrows/tab switch  "),
        Span::styled("•", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("  {} export files", merged.source_count)),
    ]));
    frame.render_widget(footer, rows[4]);
}

fn render_nav(frame: &mut Frame<'_>, area: Rect, active: ViewTab) {
    let tabs = [
        ViewTab::Overview,
        ViewTab::Daily,
        ViewTab::Monthly,
        ViewTab::Machines,
    ];
    let mut spans = Vec::new();
    for (index, tab) in tabs.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if tab == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(
            format!(" {} {} ", index + 1, tab.label()),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

struct StatsView<'a> {
    period: &'a str,
    tokens: &'a Tokens,
    cost: f64,
    active_days: usize,
    detail_label: &'a str,
    detail_value: String,
}

fn render_stats(frame: &mut Frame<'_>, area: Rect, summary: StatsView<'_>) {
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
    let stats = [
        (
            format!("{} TOKENS", summary.period),
            format_tokens(summary.tokens.effective_total()),
            Color::Cyan,
        ),
        (
            format!("{} SPEND", summary.period),
            format!("${:.2}", summary.cost),
            Color::Green,
        ),
        (
            "ACTIVE DAYS".to_string(),
            summary.active_days.to_string(),
            Color::Yellow,
        ),
        (
            summary.detail_label.to_string(),
            summary.detail_value,
            Color::Magenta,
        ),
    ];
    for (slot, (label, value, color)) in layout.iter().zip(stats) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let text = Paragraph::new(vec![
            Line::from(Span::styled(
                label,
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                value,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
        ])
        .block(block)
        .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(text, *slot);
    }
}

fn render_daily_tab(
    frame: &mut Frame<'_>,
    graph_area: Rect,
    breakdown_area: Rect,
    days: &[&DailyExport],
    totals: &PeriodTotals,
    merged: &MergedExport,
) {
    let chronological = days.iter().rev().copied().collect::<Vec<_>>();
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(graph_area);
    let charts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(panels[0]);
    render_sparkline(
        frame,
        charts[0],
        " DAILY SPEND · LAST 30 DAYS ",
        &chronological
            .iter()
            .map(|day| (clean_cost(day.cost) * 100.0).round() as u64)
            .collect::<Vec<_>>(),
        Color::Green,
    );
    render_sparkline(
        frame,
        charts[1],
        " DAILY TOKENS · LAST 30 DAYS ",
        &chronological
            .iter()
            .map(|day| day.tokens.effective_total())
            .collect::<Vec<_>>(),
        Color::Cyan,
    );
    render_token_breakdown(
        frame,
        panels[1],
        "TOKEN BREAKDOWN · 30 DAYS",
        &totals.tokens,
    );
    render_model_breakdown(frame, breakdown_area, merged);
}

fn render_monthly_tab(
    frame: &mut Frame<'_>,
    graph_area: Rect,
    breakdown_area: Rect,
    months: &[MonthlyExport],
    totals: &PeriodTotals,
    merged: &MergedExport,
) {
    let chronological = months.iter().rev().collect::<Vec<_>>();
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(graph_area);
    let charts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(panels[0]);
    render_sparkline(
        frame,
        charts[0],
        " MONTHLY SPEND ",
        &chronological
            .iter()
            .map(|month| (clean_cost(month.cost) * 100.0).round() as u64)
            .collect::<Vec<_>>(),
        Color::Green,
    );
    render_sparkline(
        frame,
        charts[1],
        " MONTHLY TOKENS ",
        &chronological
            .iter()
            .map(|month| month.tokens.effective_total())
            .collect::<Vec<_>>(),
        Color::Cyan,
    );
    render_token_breakdown(
        frame,
        panels[1],
        "TOKEN BREAKDOWN · ALL MONTHS",
        &totals.tokens,
    );
    render_model_breakdown(frame, breakdown_area, merged);
}

fn render_machine_tab(
    frame: &mut Frame<'_>,
    graph_area: Rect,
    breakdown_area: Rect,
    merged: &MergedExport,
) {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(graph_area);
    render_machine_metric(
        frame,
        panels[0],
        " MACHINE SPEND ",
        &merged.machines,
        |machine| clean_cost(machine.cost),
        Color::Green,
        |machine| format!("${:.2}", clean_cost(machine.cost)),
    );
    render_machine_metric(
        frame,
        panels[1],
        " MACHINE TOKENS ",
        &merged.machines,
        |machine| machine.tokens.effective_total() as f64,
        Color::Cyan,
        |machine| format_tokens(machine.tokens.effective_total()),
    );

    let block = Block::default()
        .title(" MACHINE DETAIL · SOURCE FILES ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(breakdown_area);
    frame.render_widget(block, breakdown_area);
    let lines = merged
        .machines
        .iter()
        .take(4)
        .map(|machine| {
            Line::from(vec![
                Span::styled(
                    format!("{:<22}", compact_label(&machine.machine, 22)),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    format!(
                        " {:>3}d  {:>5} models  {:>7} msgs",
                        machine.active_days, machine.model_count, machine.message_count
                    ),
                    Style::default().fg(Color::White),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_machine_metric<F, V>(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    machines: &[MachineUsage],
    value: F,
    color: Color,
    value_label: V,
) where
    F: Fn(&MachineUsage) -> f64,
    V: Fn(&MachineUsage) -> String,
{
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let max_value = machines.iter().map(&value).fold(0.0_f64, f64::max);
    let bar_width = inner.width.saturating_sub(32) as usize;
    let lines = machines
        .iter()
        .take(8)
        .map(|machine| {
            let current = value(machine);
            let filled = if max_value <= 0.0 {
                0
            } else {
                ((current / max_value) * bar_width as f64).round() as usize
            };
            Line::from(vec![
                Span::styled(
                    format!("{:<18}", compact_label(&machine.machine, 18)),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled("█".repeat(filled), Style::default().fg(color)),
                Span::styled(
                    format!(" {:>10}", value_label(machine)),
                    Style::default().fg(Color::White),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_sparkline(frame: &mut Frame<'_>, area: Rect, title: &str, data: &[u64], color: Color) {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let sparkline = Sparkline::default()
        .block(block)
        .data(data)
        .style(Style::default().fg(color));
    frame.render_widget(sparkline, area);
}

fn render_token_breakdown(frame: &mut Frame<'_>, area: Rect, title: &str, tokens: &Tokens) {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut buckets = vec![
        ("input", tokens.input, Color::Blue),
        ("output", tokens.output, Color::Yellow),
        ("cache read", tokens.cache_read, Color::Magenta),
        ("cache write", tokens.cache_write, Color::LightBlue),
    ];
    let explicit_total = tokens.component_total();
    let other = tokens.effective_total().saturating_sub(explicit_total);
    if other > 0 {
        buckets.push(("other", other, Color::Gray));
    }
    let max_value = buckets
        .iter()
        .map(|(_, value, _)| *value)
        .max()
        .unwrap_or(0);
    let bar_width = inner.width.saturating_sub(18) as usize;
    let lines = buckets
        .into_iter()
        .map(|(label, value, color)| {
            let filled = if max_value == 0 {
                0
            } else {
                ((value as f64 / max_value as f64) * bar_width as f64).round() as usize
            };
            Line::from(vec![
                Span::styled(format!("{label:<12}"), Style::default().fg(Color::Gray)),
                Span::styled("█".repeat(filled), Style::default().fg(color)),
                Span::styled(
                    format!(" {:>8}", format_tokens(value)),
                    Style::default().fg(Color::White),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_model_breakdown(frame: &mut Frame<'_>, area: Rect, merged: &MergedExport) {
    let block = Block::default()
        .title(" TOP MODEL SPEND · ALL EXPORTS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = merged
        .models
        .iter()
        .take(4)
        .map(|model| {
            Line::from(vec![
                Span::styled(
                    format!("{:<24}", model.model.chars().take(23).collect::<String>()),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    format!(" ${:.2}", clean_cost(model.cost)),
                    Style::default().fg(Color::Green),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_heatmap(
    frame: &mut Frame<'_>,
    area: Rect,
    weeks: &[Vec<Option<CalendarCell>>],
    graph: &tokscale_core::GraphResult,
) {
    let block = Block::default()
        .title(format!(
            " CONTRIBUTION GRAPH · {} days ",
            graph.summary.total_days
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if weeks.is_empty() || inner.height < 8 {
        frame.render_widget(Paragraph::new("No dated usage in the exports."), inner);
        return;
    }

    let cell_width = if inner.width >= (weeks.len() as u16 * 2 + 5) {
        2
    } else {
        1
    };
    let labels = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
    let mut lines = Vec::with_capacity(8);
    for (row, label) in labels.iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!("{label} "),
            Style::default().fg(Color::Gray),
        )];
        for week in weeks {
            let cell = week.get(row).and_then(Option::as_ref);
            let (text, color) = match cell {
                Some(cell) => ("█".repeat(cell_width as usize), heat_color(cell.intensity)),
                None => (" ".repeat(cell_width as usize), Color::Reset),
            };
            spans.push(Span::styled(text, Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled("less ", Style::default().fg(Color::Gray)),
        Span::styled("█", Style::default().fg(heat_color(1))),
        Span::styled("█", Style::default().fg(heat_color(2))),
        Span::styled("█", Style::default().fg(heat_color(3))),
        Span::styled("█", Style::default().fg(heat_color(4))),
        Span::styled(" more", Style::default().fg(Color::Gray)),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_models(frame: &mut Frame<'_>, area: Rect, merged: &MergedExport) {
    let block = Block::default()
        .title(" TOP MODELS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width.saturating_sub(2) as usize;
    let lines = merged
        .models
        .iter()
        .take(8)
        .map(|model| {
            let name = if model.model.len() > width.saturating_sub(18) {
                format!("{}…", &model.model[..width.saturating_sub(19).max(1)])
            } else {
                model.model.clone()
            };
            Line::from(vec![
                Span::styled(
                    format!("{name:<width$}", width = width.saturating_sub(14)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!(" ${:.2}", clean_cost(model.cost)),
                    Style::default().fg(Color::Green),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_recent_days(frame: &mut Frame<'_>, area: Rect, merged: &MergedExport) {
    let block = Block::default()
        .title(" RECENT DAYS · LAST 30 DAYS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let days = last_month_days(merged);
    let totals = sum_daily(&days);
    let mut lines = days
        .iter()
        .take(4)
        .map(|day| {
            Line::from(vec![
                Span::styled(
                    format!("{:<12}", day.date),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    format!("{:>8}", format_tokens(day.tokens.effective_total())),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("  ${:.2}", clean_cost(day.cost)),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(format!("  {} msgs", day.message_count)),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(vec![
        Span::styled(
            format!(
                "30-day total: {} tokens",
                format_tokens(totals.tokens.effective_total())
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ${:.2} spend", totals.cost),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn run_tui(merged: &MergedExport) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("stdout is not a terminal; use --no-tui when redirecting output")
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = draw_tui(&mut terminal, merged);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.machine_files.is_empty() && !args.files.is_empty() {
        bail!("use positional exports or --machine NAME=EXPORT.json, not both");
    }
    let entries = if !args.machine_files.is_empty() {
        args.machine_files
            .iter()
            .map(|spec| parse_machine_spec(spec).map(|(label, path)| (Some(label), path)))
            .collect::<Result<Vec<_>>>()?
    } else {
        let paths = if args.files.is_empty() {
            discover_files()?
        } else {
            args.files
        };
        paths.into_iter().map(|path| (None, path)).collect()
    };
    let files = read_files(&entries)?;
    let merged = merge_exports(&files);
    write_merged_export(&args.output, &merged)?;

    let date_range = merged
        .daily
        .iter()
        .filter_map(|day| NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok())
        .min()
        .zip(
            merged
                .daily
                .iter()
                .filter_map(|day| NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok())
                .max(),
        );
    let range = date_range
        .map(|(start, end)| format!("{start}..{end}"))
        .unwrap_or_else(|| "no dated usage".to_string());
    eprintln!(
        "merged {} export files -> {} | {} | {} tokens | ${:.2}",
        merged.source_count,
        args.output.display(),
        range,
        format_tokens(merged.total_tokens),
        merged.total_cost
    );

    if !args.no_tui {
        run_tui(&merged)?;
    }
    io::stdout().flush().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(total: u64) -> Tokens {
        Tokens {
            input: total / 2,
            output: total - total / 2,
            total,
            ..Tokens::default()
        }
    }

    #[test]
    fn compact_machine_labels_keep_timestamp_suffixes_distinct() {
        let first = compact_label("tokscale-export-20260721-011850", 18);
        let second = compact_label("tokscale-export-20260721-022516", 18);
        assert_ne!(first, second);
        assert_eq!(format_tokens(25_554_358_232), "25.55B");
        assert_eq!(format_tokens(25_580_375_448), "25.58B");
    }

    #[test]
    fn renders_overview_daily_monthly_and_machine_tabs() {
        use ratatui::{backend::TestBackend, Terminal};

        let daily = DailyExport {
            date: "2026-01-01".into(),
            tokens: tokens(100),
            cost: 2.0,
            message_count: 1,
            turn_count: 1,
        };
        let merged = MergedExport {
            models: Vec::new(),
            agents: Vec::new(),
            machines: vec![MachineUsage {
                machine: "test-machine".into(),
                source_file: "test.json".into(),
                tokens: tokens(100),
                cost: 2.0,
                active_days: 1,
                ..MachineUsage::default()
            }],
            daily: vec![daily],
            monthly: vec![MonthlyExport {
                month: "2026-01".into(),
                tokens: tokens(100),
                cost: 2.0,
                message_count: 1,
                turn_count: 1,
            }],
            total_tokens: 100,
            total_cost: 2.0,
            source_count: 1,
        };
        let graph = generate_graph_result(to_core_contributions(&merged.daily), 0);
        let weeks = calendar_weeks(&graph.contributions);
        let mut terminal = match Terminal::new(TestBackend::new(120, 40)) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend should initialize: {error}"),
        };
        for tab in [
            ViewTab::Overview,
            ViewTab::Daily,
            ViewTab::Monthly,
            ViewTab::Machines,
        ] {
            assert!(
                terminal
                    .draw(|frame| render(frame, &merged, &graph, &weeks, tab))
                    .is_ok(),
                "tab should render without an I/O error"
            );
        }
    }

    #[test]
    fn merges_same_model_across_machines_without_deduplication() {
        let files = vec![
            (
                PathBuf::from("a.json"),
                ExportFile {
                    models: vec![ModelExport {
                        model: "gpt-5.5".into(),
                        provider: "openai".into(),
                        client: "codex".into(),
                        tokens: tokens(10),
                        cost: 1.5,
                        performance: Performance {
                            sample_count: 1,
                            timed_tokens: 10,
                            total_duration_ms: 100,
                            ..Performance::default()
                        },
                        session_count: 2,
                    }],
                    daily: vec![DailyExport {
                        date: "2026-01-01".into(),
                        tokens: tokens(10),
                        cost: 1.5,
                        message_count: 3,
                        turn_count: 1,
                    }],
                    ..ExportFile::default()
                },
            ),
            (
                PathBuf::from("b.json"),
                ExportFile {
                    models: vec![ModelExport {
                        model: "GPT-5.5-20260101".into(),
                        provider: "openai".into(),
                        client: "pi".into(),
                        tokens: tokens(20),
                        cost: 2.5,
                        performance: Performance {
                            sample_count: 2,
                            timed_tokens: 20,
                            total_duration_ms: 300,
                            ..Performance::default()
                        },
                        session_count: 4,
                    }],
                    daily: vec![DailyExport {
                        date: "2026-01-01".into(),
                        tokens: tokens(20),
                        cost: 2.5,
                        message_count: 5,
                        turn_count: 2,
                    }],
                    ..ExportFile::default()
                },
            ),
        ];

        let merged = merge_exports(&files);
        assert_eq!(merged.machines.len(), 2);
        assert_eq!(merged.machines[0].machine, "b");
        assert_eq!(merged.machines[0].tokens.total, 20);
        assert_eq!(merged.machines[0].message_count, 5);
        assert_eq!(merged.models.len(), 1);
        assert_eq!(merged.models[0].tokens.total, 30);
        assert_eq!(merged.models[0].client, "codex, pi");
        assert_eq!(merged.models[0].session_count, 6);
        assert_eq!(merged.models[0].performance.sample_count, 3);
        assert_eq!(merged.models[0].performance.total_duration_ms, 400);
        assert_eq!(merged.daily.len(), 1);
        assert_eq!(merged.daily[0].message_count, 8);
        assert_eq!(merged.monthly[0].month, "2026-01");
        assert_eq!(merged.total_tokens, 30);
        assert!((merged.total_cost - 4.0).abs() < f64::EPSILON);
    }
}
