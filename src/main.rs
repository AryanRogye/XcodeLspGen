use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::{
    ffi::OsStr,
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(
    name = "XcodeLspGen",
    about = "Generate xcode-build-server config by scanning for Xcode projects/workspaces and selecting a scheme."
)]
struct Args {
    /// Directory to scan (defaults to current directory)
    #[arg(value_name = "DIR", default_value = ".")]
    dir: PathBuf,

    /// Prefer workspaces over projects (default true)
    #[arg(long, default_value_t = true)]
    prefer_workspace: bool,

    /// Skip interactive picker; use first scheme/target found
    #[arg(long)]
    non_interactive: bool,

    /// Provide scheme directly (skips listing/picking)
    #[arg(long)]
    scheme: Option<String>,

    /// Provide an explicit workspace path (skips scanning)
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Provide an explicit project path (skips scanning)
    #[arg(long)]
    project: Option<PathBuf>,
}

#[derive(Debug, Clone)]
enum XcodeTarget {
    Workspace(PathBuf),
    Project(PathBuf),
}

impl XcodeTarget {
    fn kind(&self) -> &'static str {
        match self {
            XcodeTarget::Workspace(_) => "workspace",
            XcodeTarget::Project(_) => "project",
        }
    }
    fn path(&self) -> &Path {
        match self {
            XcodeTarget::Workspace(p) => p.as_path(),
            XcodeTarget::Project(p) => p.as_path(),
        }
    }
}

// ====== TUI wrapper that restores terminal perfectly ======

struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable_raw_mode failed")?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen)?;
        out.flush().ok();

        let backend = CrosstermBackend::new(out);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        terminal.hide_cursor().ok();

        // restore on panic
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = Tui::restore_now();
            prev(info);
        }));

        // restore on Ctrl+C
        let _ = ctrlc::set_handler(|| {
            let _ = Tui::restore_now();
            std::process::exit(130);
        });

        Ok(Self { terminal })
    }

    fn restore_now() -> Result<()> {
        let mut out = io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(out, LeaveAlternateScreen);
        let _ = out.flush();
        Ok(())
    }

    fn exit(mut self) -> Result<()> {
        self.terminal.show_cursor().ok();
        Self::restore_now()
    }
}

// ====== Screens ======

enum Screen {
    Running {
        title: String,
        body: String,
        hint: String,
    },
    PickTarget {
        items: Vec<XcodeTarget>,
        selected: usize,
        hint: String,
    },
    PickScheme {
        target: XcodeTarget,
        items: Vec<String>,
        selected: usize,
        hint: String,
    },
    Done {
        summary: String,
        details: String,
        scroll: u16,
        hint: String,
    },
    Error {
        summary: String,
        details: String,
        scroll: u16,
        hint: String,
    },
}

// ====== main ======

fn main() {
    if let Err(e) = run() {
        eprintln!("{:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let mut tui = Tui::enter()?;

    let res = run_tui(&mut tui, &args);

    // always restore the terminal, even if res is Err
    let _ = tui.exit();
    res
}

fn run_tui(tui: &mut Tui, args: &Args) -> Result<()> {
    ensure_tool("xcodebuild")?;
    ensure_tool("xcode-build-server")?;

    // 1) resolve target
    let target = if let Some(t) = explicit_target(args)? {
        t
    } else {
        set_screen(
            tui,
            Screen::Running {
                title: "Scanning…".into(),
                body: format!("Root: {}", args.dir.display()),
                hint: "Please wait".into(),
            },
        )?;

        let mut items = scan_targets_ordered(&args.dir, args.prefer_workspace)?;

        if items.is_empty() {
            bail!("No .xcworkspace or .xcodeproj found under {}", args.dir.display());
        }

        if args.non_interactive {
            items.remove(0)
        } else {
            pick_target_screen(tui, items)?
        }
    };

    // 2) resolve scheme
    let scheme = if let Some(s) = args.scheme.clone() {
        s
    } else {
        set_screen(
            tui,
            Screen::Running {
                title: "Listing schemes…".into(),
                body: format!("Using {}: {}", target.kind(), target.path().display()),
                hint: "Please wait".into(),
            },
        )?;

        let out = run_xcodebuild_list(&target)?;
        let schemes = parse_schemes(&out);

        if schemes.is_empty() {
            bail!("No schemes found for {}", target.path().display());
        }

        if args.non_interactive {
            schemes[0].clone()
        } else {
            pick_scheme_screen(tui, target.clone(), schemes)?
        }
    };

    // 3) run xcode-build-server config
    run_config_screen(tui, target, scheme)
}

// ====== resolve / scan ======

fn explicit_target(args: &Args) -> Result<Option<XcodeTarget>> {
    if let Some(ws) = &args.workspace {
        return Ok(Some(XcodeTarget::Workspace(ws.clone())));
    }
    if let Some(pr) = &args.project {
        return Ok(Some(XcodeTarget::Project(pr.clone())));
    }
    Ok(None)
}

fn ensure_tool(name: &str) -> Result<()> {
    which::which(name).with_context(|| format!("Required tool not found in PATH: {name}"))?;
    Ok(())
}

fn scan_targets_ordered(root: &Path, prefer_workspace: bool) -> Result<Vec<XcodeTarget>> {
    let (mut workspaces, projects) = scan_for_xcode_targets(root)?;

    // Ignore auto-generated internal workspace inside .xcodeproj bundles
    workspaces.retain(|p| !p.to_string_lossy().ends_with(".xcodeproj/project.xcworkspace"));

    let mut items: Vec<XcodeTarget> = Vec::new();
    if prefer_workspace {
        items.extend(workspaces.into_iter().map(XcodeTarget::Workspace));
        items.extend(projects.into_iter().map(XcodeTarget::Project));
    } else {
        items.extend(projects.into_iter().map(XcodeTarget::Project));
        items.extend(workspaces.into_iter().map(XcodeTarget::Workspace));
    }
    Ok(items)
}

fn scan_for_xcode_targets(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut workspaces = Vec::new();
    let mut projects = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        let p = entry.path();
        match p.extension().and_then(OsStr::to_str) {
            Some("xcworkspace") => workspaces.push(p.to_path_buf()),
            Some("xcodeproj") => projects.push(p.to_path_buf()),
            _ => {}
        }
    }

    workspaces.sort();
    projects.sort();
    Ok((workspaces, projects))
}

// ====== xcodebuild / parsing ======

fn run_xcodebuild_list(target: &XcodeTarget) -> Result<String> {
    let mut cmd = Command::new("xcodebuild");
    match target {
        XcodeTarget::Workspace(p) => {
            cmd.arg("-list").arg("-workspace").arg(p);
        }
        XcodeTarget::Project(p) => {
            cmd.arg("-list").arg("-project").arg(p);
        }
    }

    let out = cmd.output().context("Failed to run xcodebuild -list")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("xcodebuild -list failed:\n{stderr}");
    }

    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn parse_schemes(list_output: &str) -> Vec<String> {
    let mut schemes = Vec::new();
    let mut in_schemes = false;

    for line in list_output.lines() {
        let trimmed = line.trim_end();

        if trimmed.trim() == "Schemes:" {
            in_schemes = true;
            continue;
        }

        if in_schemes {
            if trimmed.trim().is_empty() {
                break;
            }

            let no_indent = line.chars().next().map(|c| !c.is_whitespace()).unwrap_or(false);
            if no_indent && trimmed.ends_with(':') {
                break;
            }

            let s = trimmed.trim().to_string();
            if !s.is_empty() {
                schemes.push(s);
            }
        }
    }

    schemes
}

// ====== Run config ======

fn run_config_screen(tui: &mut Tui, target: XcodeTarget, scheme: String) -> Result<()> {
    set_screen(
        tui,
        Screen::Running {
            title: "Generating buildServer.json…".into(),
            body: format!(
                "Target: {}: {}\nScheme: {}\n\n(If this hangs, it’s xcodebuild being xcodebuild.)",
                target.kind(),
                target.path().display(),
                scheme
            ),
            hint: "Please wait".into(),
        },
    )?;

    let mut cmd = Command::new("xcode-build-server");
    cmd.arg("config");

    match &target {
        XcodeTarget::Workspace(p) => {
            cmd.arg("-workspace").arg(p);
        }
        XcodeTarget::Project(p) => {
            cmd.arg("-project").arg(p);
        }
    }

    cmd.arg("-scheme").arg(&scheme);

    let out = cmd.output().context("Failed to run xcode-build-server config")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let details = format!("{}\n{}", stdout, stderr).trim().to_string();

    if !out.status.success() {
        finish_screen(
            tui,
            Screen::Error {
                summary: "Config failed".into(),
                details,
                scroll: 0,
                hint: "Up/Down scroll • Enter/q/Esc exit".into(),
            },
        )?;
        bail!("xcode-build-server config failed");
    }

    finish_screen(
        tui,
        Screen::Done {
            summary: "Config generated successfully".into(),
            details,
            scroll: 0,
            hint: "Up/Down scroll • Enter/q/Esc exit".into(),
        },
    )?;

    Ok(())
}

// ====== Pickers (TUI) ======

fn pick_target_screen(tui: &mut Tui, items: Vec<XcodeTarget>) -> Result<XcodeTarget> {
    let mut screen = Screen::PickTarget {
        items,
        selected: 0,
        hint: "↑/↓ (or j/k) move • Enter select • q/Esc quit".into(),
    };

    loop {
        draw(tui, &mut screen)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if is_quit(&k) {
                    bail!("Cancelled");
                }

                match &mut screen {
                    Screen::PickTarget { items, selected, .. } => {
                        if is_up(&k) {
                            *selected = selected.saturating_sub(1);
                        } else if is_down(&k) {
                            *selected = (*selected + 1).min(items.len().saturating_sub(1));
                        } else if is_enter(&k) {
                            return Ok(items[*selected].clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn pick_scheme_screen(tui: &mut Tui, target: XcodeTarget, items: Vec<String>) -> Result<String> {
    let mut screen = Screen::PickScheme {
        target,
        items,
        selected: 0,
        hint: "↑/↓ (or j/k) move • Enter select • q/Esc quit".into(),
    };

    loop {
        draw(tui, &mut screen)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if is_quit(&k) {
                    bail!("Cancelled");
                }

                match &mut screen {
                    Screen::PickScheme { items, selected, .. } => {
                        if is_up(&k) {
                            *selected = selected.saturating_sub(1);
                        } else if is_down(&k) {
                            *selected = (*selected + 1).min(items.len().saturating_sub(1));
                        } else if is_enter(&k) {
                            return Ok(items[*selected].clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

// ====== Done/Error screens ======

fn finish_screen(tui: &mut Tui, mut screen: Screen) -> Result<()> {
    loop {
        draw(tui, &mut screen)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }

                if is_enter(&k) || is_quit(&k) {
                    return Ok(());
                }

                // scroll support
                let delta: i16 = if is_up(&k) { -1 } else if is_down(&k) { 1 } else { 0 };

                match &mut screen {
                    Screen::Done { scroll, .. } | Screen::Error { scroll, .. } => {
                        if delta != 0 {
                            let ns = (*scroll as i16 + delta).max(0) as u16;
                            *scroll = ns;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

// ====== Rendering ======

fn set_screen(tui: &mut Tui, mut screen: Screen) -> Result<()> {
    draw(tui, &mut screen)
}

fn draw(tui: &mut Tui, screen: &mut Screen) -> Result<()> {
    tui.terminal.draw(|f| {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(2)])
            .split(area);

        // Header
        let header = Paragraph::new("XcodeLspGen")
            .block(Block::default().borders(Borders::ALL).title("Header"))
            .wrap(Wrap { trim: true });
        f.render_widget(header, chunks[0]);

        // Body
        match screen {
            Screen::Running { title, body, .. } => {
                let p = Paragraph::new(body.clone())
                    .block(Block::default().borders(Borders::ALL).title(title.clone()))
                    .wrap(Wrap { trim: false });
                f.render_widget(p, chunks[1]);
            }
            Screen::PickTarget { items, selected, .. } => {
                let list_items: Vec<ListItem> = items
                    .iter()
                    .map(|t| ListItem::new(format!("[{}] {}", t.kind(), t.path().display())))
                    .collect();

                let list = List::new(list_items)
                    .block(Block::default().borders(Borders::ALL).title("Pick target"))
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

                // state lives locally, derived from selected index
                let mut state = ratatui::widgets::ListState::default();
                state.select(Some(*selected));
                f.render_stateful_widget(list, chunks[1], &mut state);
            }
            Screen::PickScheme { target, items, selected, .. } => {
                let list_items: Vec<ListItem> = items.iter().map(|s| ListItem::new(s.clone())).collect();

                let title = format!("Pick scheme • {}: {}", target.kind(), target.path().display());
                let list = List::new(list_items)
                    .block(Block::default().borders(Borders::ALL).title(title))
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

                let mut state = ratatui::widgets::ListState::default();
                state.select(Some(*selected));
                f.render_stateful_widget(list, chunks[1], &mut state);
            }
            Screen::Done { summary, details, scroll, .. } => {
                let p = Paragraph::new(format!("{summary}\n\n{details}"))
                    .block(Block::default().borders(Borders::ALL).title("Done"))
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0));
                f.render_widget(p, chunks[1]);
            }
            Screen::Error { summary, details, scroll, .. } => {
                let p = Paragraph::new(format!("{summary}\n\n{details}"))
                    .block(Block::default().borders(Borders::ALL).title("Error"))
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0));
                f.render_widget(p, chunks[1]);
            }
        }

        // Footer / keys
        let hint = match screen {
            Screen::Running { hint, .. } => hint.as_str(),
            Screen::PickTarget { hint, .. } => hint.as_str(),
            Screen::PickScheme { hint, .. } => hint.as_str(),
            Screen::Done { hint, .. } => hint.as_str(),
            Screen::Error { hint, .. } => hint.as_str(),
        };

        let footer = Paragraph::new(hint)
            .block(Block::default().borders(Borders::ALL).title("Keys"))
            .wrap(Wrap { trim: true });
        f.render_widget(footer, chunks[2]);
    })?;

    Ok(())
}

// ====== Key helpers ======

fn is_enter(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Enter)
}

fn is_up(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Up) || matches!(k.code, KeyCode::Char('k'))
}

fn is_down(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Down) || matches!(k.code, KeyCode::Char('j'))
}

fn is_quit(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Esc)
        || (matches!(k.code, KeyCode::Char('q')) && k.modifiers.is_empty())
        || (k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c')))
}
