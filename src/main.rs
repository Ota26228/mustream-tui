// src/main.rs

use std::{
    fs::File,
    io::{stdout, BufReader, Cursor},
    sync::{mpsc::{self, Receiver, Sender}, OnceLock},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, BorderType, Gauge, List, ListItem, ListState, Paragraph},
    Terminal,
};
use rodio::{Decoder, OutputStream, Sink};
use serde::{Deserialize, Serialize};

static SERVER_URL: OnceLock<String> = OnceLock::new();

fn get_server_url() -> &'static str {
    SERVER_URL.get_or_init(|| {
        let mut url = std::env::var("MUSTREAM_SERVER")
            .unwrap_or_else(|_| "http://localhost:3000".to_string());
        if url.ends_with('/') {
            url.pop();
        }
        url
    })
}

// --- API Models ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Song {
    id: i64,
    title: String,
    artist: String,
    album: String,
    path: String,
    duration: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct BrowseResponse {
    path: String,
    dirs: Vec<String>,
    songs: Vec<Song>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct YtResult {
    title: String,
    channel: String,
    duration: i64,
    url: String,
}

#[derive(Debug, Clone, Serialize)]
struct DownloadRequest {
    url: String,
    folder: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct MkdirRequest {
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeleteRequest {
    path: String,
}

// --- App Enums ---

#[derive(PartialEq, Clone, Copy)]
enum AppState {
    Normal,
    Playing,
    Paused,
}

#[derive(PartialEq, Clone, Copy)]
enum Repeat {
    Off,
    All,
    One,
}

#[derive(PartialEq, Clone, Copy)]
enum PanelFocus {
    Library,
    Downloader,
}

#[derive(PartialEq, Clone)]
enum InputMode {
    None,
    Search,
    Mkdir,
    DeleteConfirm(String), // holds the path to delete
}

struct LibraryItem {
    name: String,
    is_dir: bool,
    song: Option<Song>,
}

// --- Background Events ---

enum BackgroundEvent {
    BrowseResult(Result<BrowseResponse>),
    SearchResult(Result<Vec<YtResult>>),
    DownloadResult(Result<String>),
    MkdirResult(Result<()>),
    DeleteResult(Result<()>),
    PreviewBytesResult(Result<(String, Vec<u8>)>), // (title, audio_bytes)
}

// --- App State ---

struct App {
    current_path: String,
    library_items: Vec<LibraryItem>,
    library_state: ListState,

    // Audio Playback
    _stream: OutputStream,
    sink: Sink,
    currently_playing: Option<Song>,
    currently_playing_title: Option<String>, // includes YouTube preview title
    queue: Vec<Song>,
    queue_idx: usize,
    state: AppState,
    is_shuffling: bool,
    repeat: Repeat,

    // Playback duration tracking
    playback_start: Option<Instant>,
    playback_accumulated: Duration,

    // Downloader
    yt_query: String,
    yt_results: Vec<YtResult>,
    downloader_state: ListState,
    top_dirs: Vec<String>,
    selected_save_dir_idx: usize, // 0 is "Music/ (自動)", 1.. are top_dirs

    // TUI focus / Input
    active_panel: PanelFocus,
    input_mode: InputMode,
    input_value: String,
    status_message: Option<String>,
    status_expiry: Option<Instant>,

    // Loading Indicators
    library_loading: bool,
    downloader_loading: bool,
}

impl App {
    fn new(stream: OutputStream, sink: Sink) -> Self {
        Self {
            current_path: String::new(),
            library_items: Vec::new(),
            library_state: ListState::default(),

            _stream: stream,
            sink,
            currently_playing: None,
            currently_playing_title: None,
            queue: Vec::new(),
            queue_idx: 0,
            state: AppState::Normal,
            is_shuffling: false,
            repeat: Repeat::Off,

            playback_start: None,
            playback_accumulated: Duration::from_secs(0),

            yt_query: String::new(),
            yt_results: Vec::new(),
            downloader_state: ListState::default(),
            top_dirs: Vec::new(),
            selected_save_dir_idx: 0,

            active_panel: PanelFocus::Library,
            input_mode: InputMode::None,
            input_value: String::new(),
            status_message: None,
            status_expiry: None,

            library_loading: false,
            downloader_loading: false,
        }
    }

    fn set_status(&mut self, msg: String, duration_secs: u64) {
        self.status_message = Some(msg);
        self.status_expiry = Some(Instant::now() + Duration::from_secs(duration_secs));
    }

    fn check_status_expiry(&mut self) {
        if let Some(expiry) = self.status_expiry {
            if Instant::now() > expiry {
                self.status_message = None;
                self.status_expiry = None;
            }
        }
    }

    fn current_elapsed(&self) -> Duration {
        if self.state == AppState::Playing {
            if let Some(start) = self.playback_start {
                return self.playback_accumulated + start.elapsed();
            }
        }
        self.playback_accumulated
    }

    // --- Audio control ---

    fn play_music(&mut self, song: Song) -> Result<()> {
        self.sink.stop();
        let file = File::open(&song.path)?;
        let reader = BufReader::new(file);
        let source = Decoder::new(reader)?;
        self.sink.append(source);

        self.currently_playing = Some(song.clone());
        self.currently_playing_title = Some(format!("{} - {}", song.title, song.artist));
        self.state = AppState::Playing;
        self.playback_start = Some(Instant::now());
        self.playback_accumulated = Duration::from_secs(0);
        Ok(())
    }

    fn play_preview(&mut self, title: String, bytes: Vec<u8>) -> Result<()> {
        self.sink.stop();
        let cursor = Cursor::new(bytes);
        let reader = BufReader::new(cursor);
        let source = Decoder::new(reader)?;
        self.sink.append(source);

        self.currently_playing = None;
        self.currently_playing_title = Some(format!("試聴中 • {}", title));
        self.state = AppState::Playing;
        self.playback_start = Some(Instant::now());
        self.playback_accumulated = Duration::from_secs(0);
        Ok(())
    }

    fn pause_playback(&mut self) {
        if self.state == AppState::Playing {
            self.sink.pause();
            self.state = AppState::Paused;
            if let Some(start) = self.playback_start.take() {
                self.playback_accumulated += start.elapsed();
            }
        }
    }

    fn resume_playback(&mut self) {
        if self.state == AppState::Paused {
            self.sink.play();
            self.state = AppState::Playing;
            self.playback_start = Some(Instant::now());
        }
    }

    fn stop_playback(&mut self) {
        self.sink.stop();
        self.currently_playing = None;
        self.currently_playing_title = None;
        self.state = AppState::Normal;
        self.playback_start = None;
        self.playback_accumulated = Duration::from_secs(0);
    }

    fn play_next_song(&mut self) {
        if self.queue.is_empty() {
            self.stop_playback();
            return;
        }

        let next_idx = match self.repeat {
            Repeat::One => self.queue_idx,
            Repeat::All => (self.queue_idx + 1) % self.queue.len(),
            Repeat::Off => {
                let next = self.queue_idx + 1;
                if next < self.queue.len() {
                    next
                } else {
                    self.stop_playback();
                    return;
                }
            }
        };

        self.queue_idx = next_idx;
        let song = self.queue[next_idx].clone();
        if let Err(e) = self.play_music(song) {
            self.set_status(format!("Error playing next song: {}", e), 5);
            self.stop_playback();
        }
    }

    fn play_previous_song(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let prev_idx = if self.queue_idx == 0 {
            self.queue.len() - 1
        } else {
            self.queue_idx - 1
        };
        self.queue_idx = prev_idx;
        let song = self.queue[prev_idx].clone();
        if let Err(e) = self.play_music(song) {
            self.set_status(format!("Error playing previous song: {}", e), 5);
            self.stop_playback();
        }
    }

    fn start_queue_from_library(&mut self, idx: usize) {
        let songs: Vec<Song> = self
            .library_items
            .iter()
            .filter_map(|x| x.song.clone())
            .collect();
        if songs.is_empty() {
            return;
        }

        let selected_song = match &self.library_items[idx].song {
            Some(s) => s,
            None => return,
        };

        let initial_idx = songs.iter().position(|s| s.id == selected_song.id).unwrap_or(0);

        if self.is_shuffling {
            let mut other_songs = songs.clone();
            let selected = other_songs.remove(initial_idx);
            let mut rng = rand::thread_rng();
            use rand::seq::SliceRandom;
            other_songs.shuffle(&mut rng);

            let mut new_queue = vec![selected];
            new_queue.extend(other_songs);
            self.queue = new_queue;
            self.queue_idx = 0;
        } else {
            self.queue = songs;
            self.queue_idx = initial_idx;
        }

        let song = self.queue[self.queue_idx].clone();
        if let Err(e) = self.play_music(song) {
            self.set_status(format!("Error playing song: {}", e), 5);
        }
    }

    // --- Background triggers ---

    fn trigger_browse(&mut self, path: String, tx: Sender<BackgroundEvent>) {
        self.library_loading = true;
        thread::spawn(move || {
            let res = api_browse(&path);
            let _ = tx.send(BackgroundEvent::BrowseResult(res));
        });
    }

    fn trigger_search(&mut self, query: String, tx: Sender<BackgroundEvent>) {
        self.downloader_loading = true;
        thread::spawn(move || {
            let res = api_search_yt(&query);
            let _ = tx.send(BackgroundEvent::SearchResult(res));
        });
    }

    fn trigger_download(&mut self, url: String, folder: Option<String>, tx: Sender<BackgroundEvent>) {
        self.downloader_loading = true;
        thread::spawn(move || {
            let res = api_download(&url, folder.as_deref());
            let _ = tx.send(BackgroundEvent::DownloadResult(res));
        });
    }

    fn trigger_preview(&mut self, title: String, video_id: String, tx: Sender<BackgroundEvent>) {
        self.downloader_loading = true;
        thread::spawn(move || {
            let res = api_preview(&video_id).map(|bytes| (title, bytes));
            let _ = tx.send(BackgroundEvent::PreviewBytesResult(res));
        });
    }

    fn trigger_mkdir(&mut self, path: String, tx: Sender<BackgroundEvent>) {
        self.library_loading = true;
        thread::spawn(move || {
            let res = api_mkdir(&path);
            let _ = tx.send(BackgroundEvent::MkdirResult(res));
        });
    }

    fn trigger_delete(&mut self, path: String, tx: Sender<BackgroundEvent>) {
        self.library_loading = true;
        thread::spawn(move || {
            let res = api_delete(&path);
            let _ = tx.send(BackgroundEvent::DeleteResult(res));
        });
    }
}

// --- Sync API requests ---

fn api_browse(path: &str) -> Result<BrowseResponse> {
    let client = reqwest::blocking::Client::new();
    let res = client
        .get(&format!("{}/browse", get_server_url()))
        .query(&[("path", path)])
        .send()?
        .json::<BrowseResponse>()?;
    Ok(res)
}

fn api_search_yt(q: &str) -> Result<Vec<YtResult>> {
    let client = reqwest::blocking::Client::new();
    let res = client
        .get(&format!("{}/search-yt", get_server_url()))
        .query(&[("q", q)])
        .send()?
        .json::<Vec<YtResult>>()?;
    Ok(res)
}

fn api_download(url: &str, folder: Option<&str>) -> Result<String> {
    let client = reqwest::blocking::Client::new();
    let req = DownloadRequest {
        url: url.to_string(),
        folder: folder.map(String::from),
    };
    let res = client
        .post(&format!("{}/download", get_server_url()))
        .json(&req)
        .send()?;
    if res.status().is_success() {
        Ok("Download completed successfully".to_string())
    } else {
        let err_text = res.text().unwrap_or_else(|_| "Unknown error".to_string());
        Err(anyhow::anyhow!("Download failed: {}", err_text))
    }
}

fn api_preview(video_id: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::new();
    let mut res = client
        .get(&format!("{}/preview", get_server_url()))
        .query(&[("id", video_id)])
        .send()?;
    if !res.status().is_success() {
        return Err(anyhow::anyhow!(
            "Preview request failed with status: {}",
            res.status()
        ));
    }
    let mut bytes = Vec::new();
    std::io::copy(&mut res, &mut bytes)?;
    Ok(bytes)
}

fn api_mkdir(path: &str) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let req = MkdirRequest {
        path: path.to_string(),
    };
    let res = client
        .post(&format!("{}/mkdir", get_server_url()))
        .json(&req)
        .send()?;
    if res.status().is_success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("Failed to create folder"))
    }
}

fn api_delete(path: &str) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let req = DeleteRequest {
        path: path.to_string(),
    };
    let res = client
        .post(&format!("{}/delete", get_server_url()))
        .json(&req)
        .send()?;
    if res.status().is_success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("Failed to delete item"))
    }
}

// --- Helpers ---

fn extract_video_id(url: &str) -> &str {
    url.find("v=")
        .map(|i| {
            let s = &url[i + 2..];
            s.find('&').map(|j| &s[..j]).unwrap_or(s)
        })
        .unwrap_or("")
}

fn format_secs(seconds: i64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

// --- Main / Runner ---

fn main() -> Result<()> {
    // Enable raw mode & set up terminal
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Set up rodio audio output
    let (_stream, stream_handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&stream_handle)?;

    let mut app = App::new(_stream, sink);
    let (tx, rx) = mpsc::channel::<BackgroundEvent>();

    // Initial browse fetch
    app.trigger_browse(String::new(), tx.clone());

    let res = run_app(&mut terminal, &mut app, tx, rx);

    // Clean up
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    res
}

fn run_app(
    terminal: &mut Terminal<impl ratatui::backend::Backend>,
    app: &mut App,
    tx: Sender<BackgroundEvent>,
    rx: Receiver<BackgroundEvent>,
) -> Result<()> {
    loop {
        // Autoplay next song when current finishes
        if app.state == AppState::Playing && app.sink.empty() {
            app.play_next_song();
        }

        // Handle background async results
        while let Ok(bg_event) = rx.try_recv() {
            match bg_event {
                BackgroundEvent::BrowseResult(res) => {
                    app.library_loading = false;
                    match res {
                        Ok(browse) => {
                            app.current_path = browse.path;
                            let mut items = Vec::new();
                            for d in browse.dirs {
                                items.push(LibraryItem {
                                    name: d,
                                    is_dir: true,
                                    song: None,
                                });
                            }
                            for s in browse.songs {
                                items.push(LibraryItem {
                                    name: s.title.clone(),
                                    is_dir: false,
                                    song: Some(s),
                                });
                            }
                            app.library_items = items;
                            // Reset top dirs if we are at root
                            if app.current_path.is_empty() {
                                app.top_dirs = app
                                    .library_items
                                    .iter()
                                    .filter(|x| x.is_dir)
                                    .map(|x| x.name.clone())
                                    .collect();
                            }
                            // Keep selection in bounds
                            let selected = app.library_state.selected().unwrap_or(0);
                            if app.library_items.is_empty() {
                                app.library_state.select(None);
                            } else {
                                app.library_state.select(Some(selected.min(app.library_items.len() - 1)));
                            }
                        }
                        Err(e) => app.set_status(format!("Browse error: {}", e), 5),
                    }
                }
                BackgroundEvent::SearchResult(res) => {
                    app.downloader_loading = false;
                    match res {
                        Ok(results) => {
                            app.yt_results = results;
                            if app.yt_results.is_empty() {
                                app.downloader_state.select(None);
                            } else {
                                app.downloader_state.select(Some(0));
                            }
                        }
                        Err(e) => app.set_status(format!("Search error: {}", e), 5),
                    }
                }
                BackgroundEvent::DownloadResult(res) => {
                    app.downloader_loading = false;
                    match res {
                        Ok(msg) => {
                            app.set_status(msg, 5);
                            // Refresh current directory
                            app.trigger_browse(app.current_path.clone(), tx.clone());
                        }
                        Err(e) => app.set_status(format!("{}", e), 6),
                    }
                }
                BackgroundEvent::MkdirResult(res) => {
                    app.library_loading = false;
                    match res {
                        Ok(_) => {
                            app.set_status("Folder created successfully".to_string(), 4);
                            app.trigger_browse(app.current_path.clone(), tx.clone());
                        }
                        Err(e) => app.set_status(format!("Mkdir failed: {}", e), 5),
                    }
                }
                BackgroundEvent::DeleteResult(res) => {
                    app.library_loading = false;
                    match res {
                        Ok(_) => {
                            app.set_status("Deleted successfully".to_string(), 4);
                            app.trigger_browse(app.current_path.clone(), tx.clone());
                        }
                        Err(e) => app.set_status(format!("Delete failed: {}", e), 5),
                    }
                }
                BackgroundEvent::PreviewBytesResult(res) => {
                    app.downloader_loading = false;
                    match res {
                        Ok((title, bytes)) => {
                            if let Err(e) = app.play_preview(title, bytes) {
                                app.set_status(format!("Preview failed: {}", e), 5);
                            }
                        }
                        Err(e) => app.set_status(format!("Preview stream error: {}", e), 5),
                    }
                }
            }
        }

        app.check_status_expiry();

        // Draw UI
        terminal.draw(|frame| {
            let size = frame.size();

            // Core Layout
            let main_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5), // Player and Header
                    Constraint::Min(0),    // Columns
                    Constraint::Length(2), // Status & Hints
                ])
                .split(size);

            let header_area = main_layout[0];
            let columns_area = main_layout[1];
            let footer_area = main_layout[2];

            // Render Header & Player (Header area)
            let player_block = Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" mustream ", Style::default().add_modifier(Modifier::BOLD).fg(Color::Green)));
            
            let player_inner = player_block.inner(header_area);
            frame.render_widget(player_block, header_area);

            // Sub-layout for player internals: Track Title on top, Progress gauge below
            let player_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
                .split(player_inner);

            let playing_title = app.currently_playing_title.as_deref().unwrap_or("Not playing");
            let play_icon = match app.state {
                AppState::Playing => "▶ ",
                AppState::Paused => "⏸ ",
                AppState::Normal => "⏹ ",
            };

            let title_line = Line::from(vec![
                Span::styled(play_icon, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled(playing_title, Style::default().add_modifier(Modifier::BOLD)),
            ]);
            frame.render_widget(Paragraph::new(title_line), player_layout[0]);

            // Render duration bar
            let elapsed = app.current_elapsed();
            let total = app.currently_playing.as_ref().map(|s| Duration::from_secs(s.duration as u64)).unwrap_or(Duration::from_secs(0));
            
            let elapsed_secs = elapsed.as_secs();
            let total_secs = total.as_secs();
            
            let pct = if total_secs > 0 {
                (elapsed_secs * 100 / total_secs) as u16
            } else {
                0
            };
            let progress_label = format!("{} / {}", format_secs(elapsed_secs as i64), format_secs(total_secs as i64));
            
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Green).bg(Color::Rgb(30, 30, 30)))
                .percent(pct.min(100))
                .label(progress_label);
            frame.render_widget(gauge, player_layout[1]);

            // Controls status sub-bar
            let shuffle_label = if app.is_shuffling { "ON" } else { "OFF" };
            let repeat_label = match app.repeat {
                Repeat::Off => "OFF",
                Repeat::All => "ALL",
                Repeat::One => "ONE",
            };
            let controls_line = Line::from(vec![
                Span::raw("Shuffle: "),
                Span::styled(shuffle_label, Style::default().fg(if app.is_shuffling { Color::Green } else { Color::DarkGray })),
                Span::raw("  |  Repeat: "),
                Span::styled(repeat_label, Style::default().fg(match app.repeat { Repeat::Off => Color::DarkGray, _ => Color::Green })),
                Span::raw("  |  [s] Play/Pause  [Esc] Stop  [c] Toggle Play Mode  [<]/[>] Prev/Next"),
            ]);
            frame.render_widget(Paragraph::new(controls_line).style(Style::default().fg(Color::Gray)), player_layout[2]);

            // Split middle area 50/50 for Library & Downloader
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(columns_area);

            let lib_area = columns[0];
            let dl_area = columns[1];

            // Render Library Column
            let lib_border_type = if app.active_panel == PanelFocus::Library {
                BorderType::Thick
            } else {
                BorderType::Plain
            };
            let lib_border_color = if app.active_panel == PanelFocus::Library {
                Color::Cyan
            } else {
                Color::DarkGray
            };

            let lib_title = format!(" Library: Music/{} ", app.current_path);
            let lib_block = Block::default()
                .borders(Borders::ALL)
                .border_type(lib_border_type)
                .border_style(Style::default().fg(lib_border_color))
                .title(Span::styled(lib_title, Style::default().add_modifier(Modifier::BOLD)));

            if app.library_loading {
                let loading_p = Paragraph::new("\n  Loading...").style(Style::default().fg(Color::DarkGray));
                frame.render_widget(loading_p.block(lib_block), lib_area);
            } else {
                let lib_list_items: Vec<ListItem> = app.library_items
                    .iter()
                    .map(|item| {
                        if item.is_dir {
                            ListItem::new(Line::from(vec![
                                Span::styled("📁 ", Style::default().fg(Color::Cyan)),
                                Span::styled(&item.name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                            ]))
                        } else {
                            let song = item.song.as_ref().unwrap();
                            let is_playing = app.currently_playing.as_ref().map_or(false, |s| s.id == song.id);
                            
                            let (icon, style) = if is_playing {
                                ("▶ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
                            } else {
                                ("🎵 ", Style::default())
                            };

                            let duration_str = format_secs(song.duration);
                            ListItem::new(Line::from(vec![
                                Span::styled(icon, style),
                                Span::styled(&song.title, style),
                                Span::styled(format!(" - {} ", song.artist), Style::default().fg(Color::DarkGray)),
                                Span::styled(format!("({})", duration_str), Style::default().fg(Color::Gray)),
                            ]))
                        }
                    })
                    .collect();

                let lib_list = List::new(lib_list_items)
                    .block(lib_block)
                    .highlight_style(Style::default().bg(Color::Rgb(50, 50, 50)))
                    .highlight_symbol("> ");

                frame.render_stateful_widget(lib_list, lib_area, &mut app.library_state);
            }

            // Render Downloader Column
            let dl_border_type = if app.active_panel == PanelFocus::Downloader {
                BorderType::Thick
            } else {
                BorderType::Plain
            };
            let dl_border_color = if app.active_panel == PanelFocus::Downloader {
                Color::Cyan
            } else {
                Color::DarkGray
            };

            let dl_block = Block::default()
                .borders(Borders::ALL)
                .border_type(dl_border_type)
                .border_style(Style::default().fg(dl_border_color))
                .title(Span::styled(" YouTube Downloader ", Style::default().add_modifier(Modifier::BOLD)));

            let dl_inner = dl_block.inner(dl_area);
            frame.render_widget(dl_block, dl_area);

            // Sub-layout for Downloader: Search bar, Folder selector, List of results
            let dl_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Search bar
                    Constraint::Length(1), // Target folder selector
                    Constraint::Min(0),    // Results list
                ])
                .split(dl_inner);

            // Search bar
            let search_border_color = if app.input_mode == InputMode::Search { Color::Green } else { Color::DarkGray };
            let query_display = if app.yt_query.is_empty() { "(Press / to search YouTube)" } else { &app.yt_query };
            let search_para = Paragraph::new(format!(" Query: {}", query_display))
                .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(search_border_color)).title(" Search "));
            frame.render_widget(search_para, dl_layout[0]);

            // Save Folder Selector
            let save_dir_str = if app.selected_save_dir_idx == 0 {
                "Music/ (自動)".to_string()
            } else {
                format!("Music/{}/", app.top_dirs[app.selected_save_dir_idx - 1])
            };
            let save_folder_line = Line::from(vec![
                Span::styled(" Save to: ", Style::default().fg(Color::Gray)),
                Span::styled(save_dir_str, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(" (Press 'f' to cycle)", Style::default().fg(Color::DarkGray)),
            ]);
            frame.render_widget(Paragraph::new(save_folder_line), dl_layout[1]);

            // YouTube Search results list
            if app.downloader_loading {
                let loading_p = Paragraph::new("  Loading...").style(Style::default().fg(Color::DarkGray));
                frame.render_widget(loading_p, dl_layout[2]);
            } else {
                let yt_items: Vec<ListItem> = app.yt_results
                    .iter()
                    .map(|r| {
                        let duration_str = format_secs(r.duration);
                        ListItem::new(Line::from(vec![
                            Span::styled("🔍 ", Style::default().fg(Color::Red)),
                            Span::styled(&r.title, Style::default().add_modifier(Modifier::BOLD)),
                            Span::styled(format!(" - {} ", r.channel), Style::default().fg(Color::DarkGray)),
                            Span::styled(format!("({})", duration_str), Style::default().fg(Color::Gray)),
                        ]))
                    })
                    .collect();

                let yt_list = List::new(yt_items)
                    .block(Block::default().borders(Borders::NONE))
                    .highlight_style(Style::default().bg(Color::Rgb(50, 50, 50)))
                    .highlight_symbol("> ");

                frame.render_stateful_widget(yt_list, dl_layout[2], &mut app.downloader_state);
            }

            // Render Footer (Status & Input bar)
            let footer_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(1)])
                .split(footer_area);

            // Row 1: Active Input or Status message
            let row1_widget = match &app.input_mode {
                InputMode::Search => {
                    let text = format!("Search YT: {}▋", app.input_value);
                    Paragraph::new(Span::styled(text, Style::default().fg(Color::Green)))
                }
                InputMode::Mkdir => {
                    let text = format!("New Folder Name: {}▋", app.input_value);
                    Paragraph::new(Span::styled(text, Style::default().fg(Color::Green)))
                }
                InputMode::DeleteConfirm(path) => {
                    let text = format!("Delete '{}'? (y/n) ", path);
                    Paragraph::new(Span::styled(text, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)))
                }
                InputMode::None => {
                    let status = app.status_message.as_deref().unwrap_or("Ready");
                    let style = if app.status_message.is_some() {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    Paragraph::new(Span::styled(status, style))
                }
            };
            frame.render_widget(row1_widget, footer_layout[0]);

            // Row 2: Help guides
            let guide_text = match app.active_panel {
                PanelFocus::Library => {
                    "Tab: Switch Panel | j/k: Navigate | l/Enter: Enter/Play | h: Back | n: New Folder | x: Delete | q: Quit"
                }
                PanelFocus::Downloader => {
                    "Tab: Switch Panel | j/k: Navigate | /: Search YT | d/Enter: Download | p/Space: Preview | f: Cycles save folder | q: Quit"
                }
            };
            let guide_para = Paragraph::new(Span::styled(guide_text, Style::default().fg(Color::DarkGray)));
            frame.render_widget(guide_para, footer_layout[1]);
        })?;

        // Handle user input events
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Release {
                    // 1. Text input modes
                    match &app.input_mode {
                        InputMode::Search => match key.code {
                            KeyCode::Enter => {
                                let query = app.input_value.trim().to_string();
                                if !query.is_empty() {
                                    app.yt_query = query.clone();
                                    app.trigger_search(query, tx.clone());
                                }
                                app.input_mode = InputMode::None;
                            }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::None;
                            }
                            KeyCode::Backspace => {
                                app.input_value.pop();
                            }
                            KeyCode::Char(c) => {
                                app.input_value.push(c);
                            }
                            _ => {}
                        },
                        InputMode::Mkdir => match key.code {
                            KeyCode::Enter => {
                                let folder_name = app.input_value.trim().to_string();
                                if !folder_name.is_empty() {
                                    let rel_path = if app.current_path.is_empty() {
                                        folder_name
                                    } else {
                                        format!("{}/{}", app.current_path, folder_name)
                                    };
                                    app.trigger_mkdir(rel_path, tx.clone());
                                }
                                app.input_mode = InputMode::None;
                            }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::None;
                            }
                            KeyCode::Backspace => {
                                app.input_value.pop();
                            }
                            KeyCode::Char(c) => {
                                app.input_value.push(c);
                            }
                            _ => {}
                        },
                        InputMode::DeleteConfirm(path_to_delete) => match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                app.trigger_delete(path_to_delete.clone(), tx.clone());
                                app.input_mode = InputMode::None;
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.input_mode = InputMode::None;
                            }
                            _ => {}
                        },
                        InputMode::None => {
                            // 2. Global command keys
                            match key.code {
                                KeyCode::Char('q') => return Ok(()),
                                KeyCode::Tab => {
                                    app.active_panel = match app.active_panel {
                                        PanelFocus::Library => PanelFocus::Downloader,
                                        PanelFocus::Downloader => PanelFocus::Library,
                                    };
                                }
                                KeyCode::Char('s') => {
                                    match app.state {
                                        AppState::Playing => app.pause_playback(),
                                        AppState::Paused => app.resume_playback(),
                                        AppState::Normal => {
                                            // Play selected from library if focused
                                            if app.active_panel == PanelFocus::Library {
                                                if let Some(idx) = app.library_state.selected() {
                                                    if app.library_items[idx].song.is_some() {
                                                        app.start_queue_from_library(idx);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    app.stop_playback();
                                }
                                KeyCode::Char('c') => {
                                    // Toggle play modes:
                                    // shuffle -> repeat modes
                                    if app.is_shuffling {
                                        app.is_shuffling = false;
                                        app.repeat = match app.repeat {
                                            Repeat::Off => Repeat::All,
                                            Repeat::All => Repeat::One,
                                            Repeat::One => Repeat::Off,
                                        };
                                    } else {
                                        app.is_shuffling = true;
                                    }
                                }
                                KeyCode::Right | KeyCode::Char('>') => {
                                    app.play_next_song();
                                }
                                KeyCode::Left | KeyCode::Char('<') => {
                                    app.play_previous_song();
                                }
                                _ => {
                                    // 3. Panel-specific keys
                                    match app.active_panel {
                                        PanelFocus::Library => match key.code {
                                            KeyCode::Char('j') | KeyCode::Down => {
                                                if !app.library_items.is_empty() {
                                                    let i = match app.library_state.selected() {
                                                        Some(i) => if i >= app.library_items.len() - 1 { 0 } else { i + 1 },
                                                        None => 0,
                                                    };
                                                    app.library_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Char('k') | KeyCode::Up => {
                                                if !app.library_items.is_empty() {
                                                    let i = match app.library_state.selected() {
                                                        Some(i) => if i == 0 { app.library_items.len() - 1 } else { i - 1 },
                                                        None => 0,
                                                    };
                                                    app.library_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Char('l') | KeyCode::Enter => {
                                                if let Some(idx) = app.library_state.selected() {
                                                    let item = &app.library_items[idx];
                                                    if item.is_dir {
                                                        let next_path = if app.current_path.is_empty() {
                                                            item.name.clone()
                                                        } else {
                                                            format!("{}/{}", app.current_path, item.name)
                                                        };
                                                        app.trigger_browse(next_path, tx.clone());
                                                    } else {
                                                        app.start_queue_from_library(idx);
                                                    }
                                                }
                                            }
                                            KeyCode::Char('h') => {
                                                if !app.current_path.is_empty() {
                                                    let parent = if let Some(idx) = app.current_path.rfind('/') {
                                                        app.current_path[..idx].to_string()
                                                    } else {
                                                        String::new()
                                                    };
                                                    app.trigger_browse(parent, tx.clone());
                                                }
                                            }
                                            KeyCode::Char('n') => {
                                                app.input_mode = InputMode::Mkdir;
                                                app.input_value.clear();
                                            }
                                            KeyCode::Char('x') | KeyCode::Char('d') => {
                                                if let Some(idx) = app.library_state.selected() {
                                                    let item = &app.library_items[idx];
                                                    let delete_path = if item.is_dir {
                                                        if app.current_path.is_empty() {
                                                            item.name.clone()
                                                        } else {
                                                            format!("{}/{}", app.current_path, item.name)
                                                        }
                                                    } else {
                                                        item.song.as_ref().unwrap().path.clone()
                                                    };
                                                    app.input_mode = InputMode::DeleteConfirm(delete_path);
                                                }
                                            }
                                            _ => {}
                                        },
                                        PanelFocus::Downloader => match key.code {
                                            KeyCode::Char('j') | KeyCode::Down => {
                                                if !app.yt_results.is_empty() {
                                                    let i = match app.downloader_state.selected() {
                                                        Some(i) => if i >= app.yt_results.len() - 1 { 0 } else { i + 1 },
                                                        None => 0,
                                                    };
                                                    app.downloader_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Char('k') | KeyCode::Up => {
                                                if !app.yt_results.is_empty() {
                                                    let i = match app.downloader_state.selected() {
                                                        Some(i) => if i == 0 { app.yt_results.len() - 1 } else { i - 1 },
                                                        None => 0,
                                                    };
                                                    app.downloader_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Char('/') => {
                                                app.input_mode = InputMode::Search;
                                                app.input_value.clear();
                                            }
                                            KeyCode::Char('d') | KeyCode::Enter => {
                                                if let Some(idx) = app.downloader_state.selected() {
                                                    let url = app.yt_results[idx].url.clone();
                                                    let save_folder = if app.selected_save_dir_idx == 0 {
                                                        None
                                                    } else {
                                                        Some(app.top_dirs[app.selected_save_dir_idx - 1].clone())
                                                    };
                                                    app.set_status("Downloading...".to_string(), 60);
                                                    app.trigger_download(url, save_folder, tx.clone());
                                                }
                                            }
                                            KeyCode::Char('p') | KeyCode::Char(' ') => {
                                                if let Some(idx) = app.downloader_state.selected() {
                                                    let (title, url) = {
                                                        let r = &app.yt_results[idx];
                                                        (r.title.clone(), r.url.clone())
                                                    };
                                                    let video_id = extract_video_id(&url).to_string();
                                                    app.set_status("Streaming preview...".to_string(), 10);
                                                    app.trigger_preview(title, video_id, tx.clone());
                                                }
                                            }
                                            KeyCode::Char('f') => {
                                                // Cycle folder
                                                if !app.top_dirs.is_empty() {
                                                    app.selected_save_dir_idx = (app.selected_save_dir_idx + 1) % (app.top_dirs.len() + 1);
                                                } else {
                                                    app.selected_save_dir_idx = 0;
                                                }
                                            }
                                            _ => {}
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
