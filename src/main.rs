use std::path::PathBuf;

use anyhow::{bail, Result};
use chatgpt::{
    prelude::{ChatGPT, ChatGPTEngine, ModelConfiguration},
    types::{ChatMessage, ResponseChunk, Role},
};
use crossterm::{
    event::{
        self, // , DisableMouseCapture, EnableMouseCapture
        Event,
        KeyCode,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use futures::{future::FutureExt, StreamExt};
use futures_util::stream::FuturesUnordered;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};
use std::io::Write;
use tokio::{
    select,
    sync::{mpsc, watch},
};
use uuid::Uuid;

#[derive(Debug)]
enum UiMode {
    ChatSelection,
    Chat,
    // Help,
}

#[derive(Debug)]
enum ChatGPTMessageChunkType {
    Chat { chat_id: Uuid, message_id: usize },
    ChatTitle { chat_id: Uuid },
}

#[derive(Debug)]
enum AppMessage {
    KeyEvent(crossterm::event::KeyEvent),
    // KeyEvent(crossterm::event::KeyEvent),
    ResizeEvent, // TODO dimensions
    ChatGPTMessageChunkReceived { message_type: ChatGPTMessageChunkType, chunk: ResponseChunk },
}

#[derive(Debug)]
enum ChatGPTMessage {
    ChatRequest { id: Uuid, messages: Vec<ChatMessage> },
    // TODO make system_message configurable? Or just hardcode it?
    ChatTitleRequest { id: Uuid, system_message: String },
    // TODO
    // ChangeModelConfiguration(ModelConfiguration),
}

// TODO support more than one reply count
#[derive(Debug, Serialize, Deserialize)]
struct Chat {
    history: Vec<ChatMessage>,
    title: String,
    /// Current value of the input box
    input: String,
    input_pos: usize,
    scroll: usize,
    id: Uuid,
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct State {
    chats: Vec<Chat>,
    current_chat_id: Option<Uuid>,
}

/// App holds the state of the application
struct App {
    ui_mode: UiMode,

    state: State,

    // This is quite a hack to get termimad working with ratatui,
    // as termimad directly writes to stdout, while ratatui is buffered
    // so what happens here, is that this function is run *after* ratatui has drawn everything to stdout and flushed it's output
    // and termimad writes over it,
    // this has the drawback that no stuff above termimad (chat-area) can be drawn in the main ui function...
    // also setting the cursor anywhere doesn't really work that way anymore,
    // so this is technical debt that has to be handled at some time...
    draw_chat_area: Option<Box<dyn FnOnce(usize) -> usize + Send>>,

    app_message_receiver: mpsc::UnboundedReceiver<AppMessage>,
    chatgpt_message_sender: mpsc::UnboundedSender<ChatGPTMessage>,
    quit_signal_sender: watch::Sender<()>,
}

impl App {
    pub fn new(
        state: State,
        app_message_receiver: mpsc::UnboundedReceiver<AppMessage>,
        chatgpt_message_sender: mpsc::UnboundedSender<ChatGPTMessage>,
        quit_signal_sender: watch::Sender<()>,
    ) -> Self {
        App {
            state,
            ui_mode: UiMode::ChatSelection,
            app_message_receiver,
            chatgpt_message_sender,
            quit_signal_sender,
            draw_chat_area: None,
        }
    }

    pub fn save_state(&self) -> Result<()> {
        let state_dir = PathBuf::from(std::env::var("HOME")?).join(".local/share/chatgpt");
        std::fs::create_dir_all(&state_dir)?;

        let state = toml::to_string_pretty(&self.state)?;

        let mut f = std::fs::File::create(state_dir.join("state.toml"))?;
        Ok(f.write_all(state.as_bytes())?)
    }

    pub fn current_chat_idx(&self) -> Option<usize> {
        self.state.chats.iter().enumerate().find_map(|(idx, chat)| {
            if Some(chat.id) == self.state.current_chat_id {
                Some(idx)
            } else {
                None
            }
        })
    }
    pub fn current_chat(&self) -> Option<&Chat> {
        self.state.current_chat_id.and_then(|id| self.chat(id))
    }
    pub fn current_chat_mut(&mut self) -> Option<&mut Chat> {
        self.state.current_chat_id.and_then(|id| self.chat_mut(id))
    }
    pub fn chat_mut(&mut self, id: Uuid) -> Option<&mut Chat> {
        self.state.chats.iter_mut().find(|chat| chat.id == id)
    }
    pub fn chat(&self, id: Uuid) -> Option<&Chat> {
        self.state.chats.iter().find(|chat| chat.id == id)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // TODO support cross platform config/state loading
    let mut state: State =
        std::fs::read_to_string(PathBuf::from(std::env::var("HOME")?).join(".local/share/chatgpt/state.toml"))
            .map_err(anyhow::Error::from)
            .and_then(|s| Ok(toml::from_str(&s)?))
            .unwrap_or_default();

    let api_key =
        std::fs::read_to_string(PathBuf::from(std::env::var("HOME")?).join(".config/chatgpt/api-key"))?.trim().into();

    // let config = std::fs::read_to_string(PathBuf::from(std::env::var("HOME")?).join(".config/chatgpt/config.toml"))?
    //     .trim()
    //     .into();

    // state.current_chat_id = None;
    let id = Uuid::new_v4();
    state.current_chat_id = Some(id);

    state.chats.push(Chat {
        title: id.to_string(),
        id,
        scroll: 0,
        input: String::new(),
        input_pos: 0,
        history: vec![ChatMessage {
            role: chatgpt::types::Role::System,
            content: format!(
                "You are ChatGPT, an AI model developed by OpenAI. \
                    Answer as concisely as possible. Today is: {0}",
                chrono::Local::now().format("%d/%m/%Y %H:%M")
            ),
        }],
    });

    let (app_message_sender, app_message_receiver) = mpsc::unbounded_channel();
    let (chatgpt_message_sender, chatgpt_message_receiver) = mpsc::unbounded_channel();
    let (quit_signal_sender, quit_signal_receiver) = watch::channel(());

    let mut app = App::new(state, app_message_receiver, chatgpt_message_sender, quit_signal_sender);

    app.ui_mode = UiMode::Chat;

    let mut set = tokio::task::JoinSet::new();
    set.spawn(run_app(app));
    set.spawn(handle_input(app_message_sender.clone(), quit_signal_receiver.clone()));
    set.spawn(handle_chatgpt(api_key, app_message_sender, chatgpt_message_receiver, quit_signal_receiver));

    while let Some(res) = set.join_next().await {
        res??;
    }
    Ok(())
}

async fn run_app(mut app: App) -> Result<()> {
    // setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen,)?; // EnableMouseCapture
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // main loop
    loop {
        terminal.draw(|f| ui(f, &mut app))?;
        // let the hackery begin
        if let Some(draw_chat_area) = app.draw_chat_area.take() {
            let scroll = if let Some(chat) = app.current_chat() { chat.scroll } else { 0 };
            let scroll = draw_chat_area(scroll);
            if let Some(chat) = app.current_chat_mut() {
                chat.scroll = scroll;
            }
            if matches!(app.ui_mode, UiMode::Chat) {
                if let Some(chat) = app.current_chat() {
                    let wrapped_input =
                        textwrap::wrap(chat.input.trim(), textwrap::Options::new(terminal.size()?.width as usize));
                    let trailing_whitespace_count = chat.input.len() - chat.input.trim_end().len();
                    let term_height = terminal.size()?.height;
                    terminal.backend_mut().queue(crossterm::cursor::MoveTo(
                        wrapped_input[wrapped_input.len() - 1].len() as u16 + trailing_whitespace_count as u16,
                        term_height + wrapped_input.len() as u16 - 1 - 4,
                    ))?;
                    terminal.backend_mut().queue(crossterm::cursor::Show)?;
                    terminal.backend_mut().queue(crossterm::cursor::EnableBlinking)?;
                    std::io::Write::flush(&mut terminal.backend_mut())?;
                }
            } else {
                terminal.backend_mut().queue(crossterm::cursor::Hide)?;
                std::io::Write::flush(&mut terminal.backend_mut())?;
            }
        }

        match app.app_message_receiver.recv().await {
            Some(AppMessage::KeyEvent(key)) => match (&app.ui_mode, key.code) {
                (UiMode::ChatSelection, KeyCode::Enter) => {
                    app.ui_mode = UiMode::Chat;
                }
                (UiMode::ChatSelection, KeyCode::Up) => {
                    if let Some(idx) = app.current_chat_idx() {
                        let new_idx = if idx == app.state.chats.len() - 1 { 0 } else { idx + 1 };
                        app.state.current_chat_id = Some(app.state.chats[new_idx].id);
                    } else if !app.state.chats.is_empty() {
                        app.state.current_chat_id = Some(app.state.chats[app.state.chats.len() - 1].id);
                    }
                }
                (UiMode::ChatSelection, KeyCode::Down) => {
                    if let Some(idx) = app.current_chat_idx() {
                        let new_idx = if idx == 0 { app.state.chats.len() - 1 } else { idx - 1 };
                        app.state.current_chat_id = Some(app.state.chats[new_idx].id);
                    } else if !app.state.chats.is_empty() {
                        app.state.current_chat_id = Some(app.state.chats[app.state.chats.len() - 1].id);
                    }
                }
                // KeyCode::Char('h') => {
                //     app.ui_mode = UiMode::Help;
                // }
                (UiMode::ChatSelection, KeyCode::Char('n')) => {
                    let id = Uuid::new_v4();
                    app.state.current_chat_id = Some(id);

                    app.state.chats.push(Chat {
                        title: id.to_string(),
                        id,
                        scroll: 0,
                        input: String::new(),
                        input_pos: 0,
                        history: vec![ChatMessage {
                            role: chatgpt::types::Role::System,
                            content: format!(
                                "You are ChatGPT, an AI model developed by OpenAI. \
                                    Answer as concisely as possible. Today is: {0}",
                                chrono::Local::now().format("%d/%m/%Y %H:%M")
                            ),
                        }],
                    });
                }
                (UiMode::ChatSelection, KeyCode::Esc | KeyCode::Char('q')) => {
                    app.quit_signal_sender.send(()).ok();
                    break;
                }
                (UiMode::ChatSelection, _) => {}
                (UiMode::Chat, KeyCode::Enter) => {
                    if app.state.current_chat_id.is_none() {
                        let id = Uuid::new_v4();
                        app.state.current_chat_id = Some(id);

                        app.state.chats.push(Chat {
                            title: id.to_string(),
                            id,
                            scroll: 0,
                            input: String::new(),
                            input_pos: 0,
                            history: vec![ChatMessage {
                                role: chatgpt::types::Role::System,
                                content: format!(
                                    "You are ChatGPT, an AI model developed by OpenAI. \
                                        Answer as concisely as possible. Today is: {0}",
                                    chrono::Local::now().format("%d/%m/%Y %H:%M")
                                ),
                            }],
                        });
                    }

                    let chat_id = app.state.current_chat_id.unwrap();

                    if let Some(chat) = app.state.chats.iter_mut().find(|c| c.id == chat_id) {
                        chat.history.push(ChatMessage {
                            role: chatgpt::types::Role::User,
                            content: chat.input.drain(..).collect(),
                        });
                        chat.input_pos = 0;

                        app.chatgpt_message_sender
                            .send(ChatGPTMessage::ChatRequest { id: chat_id, messages: chat.history.clone() })
                            .ok();
                    } else {
                        bail!("There's no chat with id: '{}'", chat_id)
                    }
                    app.save_state()?;
                }
                (UiMode::Chat, KeyCode::Char(c)) => {
                    if let Some(chat) = app.current_chat_mut() {
                        chat.input.push(c);
                        chat.input_pos += 1;
                    }
                }
                (UiMode::Chat, KeyCode::Backspace) => {
                    if let Some(chat) = app.current_chat_mut() {
                        chat.input.pop();
                        chat.input_pos -= 1;
                    }
                }
                (UiMode::Chat, KeyCode::Esc) => {
                    app.ui_mode = UiMode::ChatSelection;
                    if let Some(chat) = app.current_chat() {
                        if chat.history.len() > 1 {
                            app.save_state()?;
                        }
                    }
                }
                (UiMode::Chat, KeyCode::Up) => {
                    if let Some(chat) = app.current_chat_mut() {
                        chat.scroll = chat.scroll.saturating_sub(1);
                    }
                }
                (UiMode::Chat, KeyCode::Down) => {
                    if let Some(chat) = app.current_chat_mut() {
                        chat.scroll = chat.scroll.saturating_add(1);
                    }
                }
                (UiMode::Chat, _) => {}
            },
            Some(AppMessage::ChatGPTMessageChunkReceived {
                message_type: ChatGPTMessageChunkType::Chat { chat_id, message_id },
                chunk,
            }) => {
                // TODO handle properly (delete stream),
                // when a chat may have been deleted while still transferring chunks for this chat still...
                // not sure, if it's worth the effort though

                if let Some(chat) = app.chat_mut(chat_id) {
                    match chunk {
                        ResponseChunk::BeginResponse { role, .. } => {
                            chat.history.push(ChatMessage { role, content: String::new() });
                        }
                        ResponseChunk::Content { delta, .. } => {
                            chat.history[message_id].content += &delta;
                        }
                        ResponseChunk::Done => {
                            app.save_state()?;

                            let chat = app.chat(chat_id).expect("The chat doesn't exist");
                            // create a title for that chat
                            if chat.history.len() == 3 {
                                let mut system_message = "Provide a useful and very descriptive title with max 4 words for the following chat,\
                                    where System: <text> at the beginning of a line describes what you, the Assistant should be, and
                                    User: <text> at the beginning of a line denotes what the User asked,\
                                    and Assistant: <text> at the beginning of a line denotes what your answer is,\
                                    everything after the following colon is the chat:\n".to_string();

                                for message in &chat.history {
                                    system_message += &format!("{:?}: {}", message.role, message.content)
                                }

                                app.chatgpt_message_sender
                                    .send(ChatGPTMessage::ChatTitleRequest { id: chat.id, system_message })
                                    .ok();
                            }
                        }
                        // TODO do here anything at all?
                        ResponseChunk::CloseResponse { .. } => {}
                    }
                }
            }
            Some(AppMessage::ChatGPTMessageChunkReceived {
                message_type: ChatGPTMessageChunkType::ChatTitle { chat_id },
                chunk,
            }) => {
                if let Some(chat) = app.chat_mut(chat_id) {
                    match chunk {
                        ResponseChunk::BeginResponse { .. } => {
                            chat.title = String::new();
                        }
                        ResponseChunk::Content { delta, .. } => {
                            chat.title += &delta;
                        }
                        ResponseChunk::Done => {
                            app.save_state()?;
                        }
                        _ => {}
                    }
                }
            }
            Some(AppMessage::ResizeEvent) => {} // resizes automatically the next time it renders
            None => {
                app.quit_signal_sender.send(()).ok();
                break;
            }
        }
    }

    // cleanup
    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen,)?; // DisableMouseCapture
    terminal.show_cursor()?;
    Ok(())
}

async fn handle_input(
    message_sender: mpsc::UnboundedSender<AppMessage>,
    mut quit_signal_receiver: watch::Receiver<()>,
) -> Result<()> {
    let mut reader = event::EventStream::new();
    loop {
        let event = reader.next().fuse();

        select! {
            Ok(()) = quit_signal_receiver.changed() =>  return Ok(()),
            maybe_event = event => {
                match maybe_event {
                    Some(res) =>  match res? {
                        Event::Key(key_event) => {
                            message_sender.send(AppMessage::KeyEvent(key_event)).expect("receiver closed unexpectadly");
                        }
                        // Event::FocusGained => todo!(),
                        // Event::FocusLost => todo!(),
                        // Event::Mouse(_) => todo!(),
                        // Event::Paste(_) => todo!(),
                        Event::Resize(_, _) => {
                            message_sender.send(AppMessage::ResizeEvent).expect("receiver closed unexpectadly");
                        },
                        _ => {}
                    },
                    None => return Ok(()),
                }
            }
        };
    }
}

async fn handle_chatgpt(
    api_key: String,
    app_message_sender: mpsc::UnboundedSender<AppMessage>,
    mut chat_message_receiver: mpsc::UnboundedReceiver<ChatGPTMessage>,
    mut quit_signal_receiver: watch::Receiver<()>,
) -> Result<()> {
    // Creating a client
    // TODO support other OS than linux
    let client =
        ChatGPT::new_with_config(api_key, ModelConfiguration { engine: ChatGPTEngine::Gpt4, ..Default::default() })?;

    let mut open_streams = FuturesUnordered::new();

    loop {
        select! {
            Ok(()) = quit_signal_receiver.changed() =>  return Ok(()),
            Some(message) = chat_message_receiver.recv() => {
                match message {
                    ChatGPTMessage::ChatRequest { id, messages } => {
                        let message_id = messages.len();
                        let stream = client.send_history_streaming(&messages).await?;
                        let app_message_sender = app_message_sender.clone();
                        let chat_stream = stream.for_each(move |chunk| {
                            app_message_sender
                                .send(AppMessage::ChatGPTMessageChunkReceived {
                                    message_type: ChatGPTMessageChunkType::Chat { chat_id: id, message_id },
                                    chunk,
                                })
                                .ok();
                            futures::future::ready(())
                        });
                        open_streams.push(chat_stream.boxed());
                    }
                    ChatGPTMessage::ChatTitleRequest { id, system_message } => {
                        let message = vec![ChatMessage { role: Role::System, content: system_message }];
                        let stream = client.send_history_streaming(&message).await?;
                        let app_message_sender = app_message_sender.clone();
                        let chat_stream = stream.for_each(move |chunk| {
                            app_message_sender
                                .send(AppMessage::ChatGPTMessageChunkReceived {
                                    message_type: ChatGPTMessageChunkType::ChatTitle { chat_id: id },
                                    chunk,
                                })
                                .ok();
                            futures::future::ready(())
                        });
                        open_streams.push(chat_stream.boxed());
                    }
                    // ChatGPTMessage::ChangeModelConfiguration(config) => {
                    //     client = ChatGPT::new_with_config(key, config)?;
                    // }
                }
            },
            _ = open_streams.next() => {},

        }
    }
}

/// helper function to create a centered rect using up certain percentage of the available rect `r`
// fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
//     let popup_layout = Layout::default()
//         .direction(Direction::Vertical)
//         .constraints(
//             [
//                 Constraint::Percentage((100 - percent_y) / 2),
//                 Constraint::Percentage(percent_y),
//                 Constraint::Percentage((100 - percent_y) / 2),
//             ]
//             .as_ref(),
//         )
//         .split(r);

//     Layout::default()
//         .direction(Direction::Horizontal)
//         .constraints(
//             [
//                 Constraint::Percentage((100 - percent_x) / 2),
//                 Constraint::Percentage(percent_x),
//                 Constraint::Percentage((100 - percent_x) / 2),
//             ]
//             .as_ref(),
//         )
//         .split(popup_layout[1])[1]
// }

fn ui<B: Backend>(f: &mut Frame<B>, app: &mut App) {
    f.render_widget(ratatui::widgets::Clear, f.size()); //this clears out the background
    let in_chat_mode = matches!(app.ui_mode, UiMode::Chat);
    let mut constraints = Vec::new();
    // chat selection constraint
    if !in_chat_mode {
        if let Some(max_title_length) = app.state.chats.iter().map(|c| c.title.len()).max() {
            let max_title_length = max_title_length as u16 + 5; // some extra padding because of selection marker
            let screen_width = f.size().width;
            let chat_selection_constraint = if max_title_length < std::cmp::max(screen_width, 100) * 3 / 10 {
                Constraint::Length(std::cmp::max(4, max_title_length))
            } else if screen_width > 100 {
                Constraint::Percentage(30)
            } else {
                Constraint::Length(30)
            };
            constraints.push(chat_selection_constraint);
        } else {
            constraints.push(Constraint::Length(30));
        }
    }
    let draw_chat_ui = f.size().width > 42 || in_chat_mode;
    // chat constraint
    if draw_chat_ui {
        constraints.push(Constraint::Min(1));
    }
    let chunks = Layout::default().direction(Direction::Horizontal).constraints(constraints).split(f.size());

    if !in_chat_mode {
        chat_selection_ui(f, app, chunks[0]);
    }

    if draw_chat_ui {
        chat_ui(f, app, chunks[if !in_chat_mode { 1 } else { 0 }]);
    }

    // Something like this has its problems because termimad overwrites this at a later step...
    // if matches!(app.ui_mode, UiMode::Help) {
    //     let block =
    //         Block::default().title("Help").borders(Borders::ALL).border_type(ratatui::widgets::BorderType::Rounded);
    //     let area = centered_rect(90, 90, f.size());
    //     f.render_widget(ratatui::widgets::Clear, area); //this clears out the background
    //     f.render_widget(block, area);
    // }
}

fn chat_selection_ui<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let chat_titles: Vec<ListItem> =
        app.state.chats.iter().rev().map(|chat| ListItem::new(chat.title.clone())).collect();
    let mut state = ListState::default();
    let selected_chat = app.state.current_chat_id.and_then(|current_chat_id| {
        app.state
            .chats
            .iter()
            .rev()
            .enumerate()
            .find_map(|(i, chat)| if chat.id == current_chat_id { Some(i) } else { None })
    });
    state.select(selected_chat);

    let chats = List::new(chat_titles)
        .block(Block::default().borders(Borders::ALL).title("Chats"))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    f.render_stateful_widget(chats, area, &mut state);
}

fn make_skin() -> termimad::MadSkin {
    let mut skin = termimad::MadSkin::default();
    skin.table.align = termimad::Alignment::Center;
    skin.set_headers_fg(termimad::crossterm::style::Color::DarkYellow);
    skin.bold.set_fg(termimad::crossterm::style::Color::DarkYellow);
    skin.italic.set_fg(termimad::crossterm::style::Color::DarkMagenta);
    skin.scrollbar.thumb.set_fg(termimad::crossterm::style::Color::DarkYellow);
    skin.code_block.align = termimad::Alignment::Left;
    skin
}

fn chat_ui<B: Backend>(f: &mut Frame<B>, app: &mut App, area: Rect) {
    let in_chat_mode = matches!(app.ui_mode, UiMode::Chat);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if in_chat_mode {
            vec![Constraint::Min(1), Constraint::Length(5)]
        } else {
            vec![Constraint::Min(1)]
        })
        .split(area);

    if in_chat_mode {
        let wrapped_input = textwrap::wrap(
            app.current_chat().map(|c| c.input.as_str()).unwrap_or(""),
            textwrap::Options::new(chunks[1].width as usize),
        )
        .join("\n");
        let input = Paragraph::new(wrapped_input.as_ref())
            .style(Style::default().fg(Color::Blue))
            .block(Block::default().borders(Borders::TOP.union(Borders::BOTTOM)).title("Input"))
            .alignment(ratatui::layout::Alignment::Left);
        f.render_widget(input, chunks[1]);
    }

    let messages: String =
        app.current_chat_idx().map(|i| app.state.chats[i].history.iter()).into_iter().flatten().fold(
            String::new(),
            |mut acc, message| {
                acc += &format!("\n\n## {:?}:\n\n{}", message.role, message.content);
                acc
            },
        );

    let (borders, message_area) = if !in_chat_mode {
        (
            Borders::ALL,
            termimad::Area::new(
                chunks[0].x.saturating_add(1),
                chunks[0].y.saturating_add(1),
                std::cmp::max(1, chunks[0].width.saturating_sub(2)),
                std::cmp::max(1, chunks[0].height.saturating_sub(2)),
            ),
        )
    } else {
        (
            Borders::TOP.union(Borders::BOTTOM),
            termimad::Area::new(chunks[0].x, chunks[0].y + 1, chunks[0].width, chunks[0].height - 2),
        )
    };

    let message_area_border = Block::default()
        .borders(borders)
        .style(Style::default().bg(Color::Black))
        .title(app.current_chat().map(|c| c.title.as_str()).unwrap_or("Messages"));
    f.render_widget(message_area_border, chunks[0]);

    app.draw_chat_area = Some(Box::new(move |scroll| {
        let mut view = termimad::MadView::from(messages, message_area, make_skin());
        let mut w = std::io::stdout();
        // view.scroll = scroll;
        view.try_scroll_lines(scroll as i32);
        view.write_on(&mut w).ok();
        w.flush().ok();
        view.scroll
    }));
}
