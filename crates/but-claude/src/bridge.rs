//! Claude bridge.
//!
//! The goal of this module is to provide the frontend with a way of talking
//! claude code.
//!
//! There have been three different methods for building this proposed:
//!
//! Streamed input & output
//! - This might give us a little bit more control and have the ability to send
//!   stop signals that are more graceful than just aborting the process.
//! - This does require the management of long lived child processes.
//! - **This is currently broken**
//!
//! Streamed output
//! - It would be curious how this plays into features like queuing multiple
//!   messages.
//!
//! Streamed output and managing tool call output
//! - This might give us more flexabiity in the long run, but initially seems
//!   more complex with more unknowns.

use crate::{
    ClaudeMessage, ClaudeMessageContent, UserInput,
    claude_config::{fmt_claude_mcp, fmt_claude_settings},
    db,
    rules::{create_claude_assignment_rule, list_claude_assignment_rules},
};
use anyhow::{Result, bail};
use but_broadcaster::{Broadcaster, FrontendEvent};
use but_workspace::StackId;
use gitbutler_command_context::CommandContext;
use serde_json::json;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, PipeReader, Read as _},
    process::ExitStatus,
    sync::Arc,
};
use tokio::{
    process::{Child, Command},
    sync::{
        Mutex,
        mpsc::{UnboundedSender, unbounded_channel},
    },
};

/// Holds the CC instances. Currently keyed by stackId, since our current model
/// assumes one CC per stack at any given time.
pub struct Claudes {
    /// A set that contains all the currently running requests
    requests: Mutex<HashMap<StackId, Arc<Claude>>>,
}

pub struct Claude {
    kill: UnboundedSender<()>,
}

impl Claudes {
    pub fn new() -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
        }
    }

    pub async fn send_message(
        &self,
        ctx: Arc<Mutex<CommandContext>>,
        broadcaster: Arc<tokio::sync::Mutex<Broadcaster>>,
        stack_id: StackId,
        message: &str,
    ) -> Result<()> {
        if self.requests.lock().await.contains_key(&stack_id) {
            bail!("Claude is thinking, back off!!!")
        } else {
            self.spawn_claude(ctx, broadcaster, stack_id, message.to_owned())
                .await?
        };

        Ok(())
    }

    pub fn get_messages(
        &self,
        ctx: &mut CommandContext,
        stack_id: StackId,
    ) -> Result<Vec<ClaudeMessage>> {
        let rule = list_claude_assignment_rules(ctx)?
            .into_iter()
            .find(|rule| rule.stack_id == stack_id);
        if let Some(rule) = rule {
            let messages = db::list_messages_by_session(ctx, rule.session_id)?;
            Ok(messages)
        } else {
            Ok(vec![])
        }
    }

    /// Cancel a running Claude session for the given stack
    pub async fn cancel_session(&self, stack_id: StackId) -> Result<bool> {
        let requests = self.requests.lock().await;
        if let Some(claude) = requests.get(&stack_id) {
            // Send the kill signal
            claude
                .kill
                .send(())
                .map_err(|_| anyhow::anyhow!("Failed to send kill signal"))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn spawn_claude(
        &self,
        ctx: Arc<Mutex<CommandContext>>,
        broadcaster: Arc<tokio::sync::Mutex<Broadcaster>>,
        stack_id: StackId,
        message: String,
    ) -> Result<()> {
        let (send_kill, mut recv_kill) = unbounded_channel();
        self.requests
            .lock()
            .await
            .insert(stack_id, Arc::new(Claude { kill: send_kill }));

        // We're also making the bold assumption that if we can find the
        // transcript, that a session was created. This is _not_ the best
        // way to do this.
        //
        // https://github.com/anthropics/claude-code/issues/5161 could
        // simplify this
        let rule = {
            let mut ctx = ctx.lock().await;
            list_claude_assignment_rules(&mut ctx)?
                .into_iter()
                .find(|rule| rule.stack_id == stack_id)
        };

        let create_new = rule.is_none();
        let session_id = rule.map(|r| r.session_id).unwrap_or(uuid::Uuid::new_v4());

        let broadcaster = broadcaster.clone();

        let session = upsert_session(ctx.clone(), session_id, stack_id).await?;
        {
            let mut ctx = ctx.lock().await;
            send_claude_message(
                &mut ctx,
                broadcaster.clone(),
                session_id,
                stack_id,
                ClaudeMessageContent::UserInput(UserInput {
                    message: message.to_owned(),
                }),
            )
            .await?;
        }
        let (read_stdout, writer) = std::io::pipe()?;
        let response_streamer = spawn_response_streaming(
            ctx.clone(),
            broadcaster.clone(),
            read_stdout,
            session_id,
            stack_id,
        );

        let (read_stderr, write_stderr) = std::io::pipe()?;
        // Clone so the reference to ctx can be immediatly dropped
        let project = ctx.lock().await.project().clone();
        let mut handle = spawn_command(
            message,
            create_new,
            writer,
            write_stderr,
            session,
            project.path.clone(),
            ctx.clone(),
        )
        .await?;
        let cmd_exit = tokio::select! {
            status = handle.wait() => Exit::WithStatus(status),
            _ = recv_kill.recv() => Exit::ByUser
        };
        // My understanding is that it is not great to abort things like this,
        // but it's "good enough" for now.
        response_streamer.abort();
        self.requests.lock().await.remove(&stack_id);

        handle_exit(
            ctx,
            broadcaster,
            stack_id,
            session_id,
            read_stderr,
            handle,
            cmd_exit,
        )
        .await?;

        Ok(())
    }
}

async fn handle_exit(
    ctx: Arc<Mutex<CommandContext>>,
    broadcaster: Arc<Mutex<Broadcaster>>,
    stack_id: but_core::Id<'S'>,
    session_id: uuid::Uuid,
    mut read_stderr: PipeReader,
    mut handle: Child,
    cmd_exit: Exit,
) -> Result<(), anyhow::Error> {
    match cmd_exit {
        Exit::WithStatus(exit_status) => {
            let exit_status = exit_status?;
            let mut buf = String::new();
            read_stderr.read_to_string(&mut buf)?;
            let mut ctx = ctx.lock().await;
            send_claude_message(
                &mut ctx,
                broadcaster.clone(),
                session_id,
                stack_id,
                ClaudeMessageContent::GitButlerMessage(crate::GitButlerMessage::ClaudeExit {
                    code: exit_status.code().unwrap_or(0),
                    message: buf.clone(),
                }),
            )
            .await?;
        }
        Exit::ByUser => {
            // On *nix try to kill claude more gently.
            #[cfg(unix)]
            {
                use nix::sys::signal::{self, Signal};
                use nix::unistd::Pid;
                if let Some(pid) = handle.id() {
                    signal::kill(Pid::from_raw(pid as i32), Signal::SIGINT)?;
                    handle.wait().await?;
                } else {
                    handle.kill().await?;
                }
            }
            #[cfg(not(unix))]
            {
                handle.kill().await?;
            }
            let mut ctx = ctx.lock().await;
            send_claude_message(
                &mut ctx,
                broadcaster.clone(),
                session_id,
                stack_id,
                ClaudeMessageContent::GitButlerMessage(crate::GitButlerMessage::UserAbort),
            )
            .await?;
        }
    }
    Ok(())
}

enum Exit {
    WithStatus(std::io::Result<ExitStatus>),
    ByUser,
}

/// Spawns the actual claude code command
async fn spawn_command(
    message: String,
    create_new: bool,
    writer: std::io::PipeWriter,
    write_stderr: std::io::PipeWriter,
    session: crate::ClaudeSession,
    project_path: std::path::PathBuf,
    ctx: Arc<Mutex<CommandContext>>,
) -> Result<Child> {
    // Write and obtain our own claude hooks path.
    let settings = fmt_claude_settings()?;
    let mcp_config = fmt_claude_mcp()?;

    let claude_executable = ctx.lock().await.app_settings().claude.executable.clone();
    let mut command = Command::new(claude_executable);
    command.stdout(writer);
    command.stderr(write_stderr);
    command.current_dir(&project_path);
    command.args([
        "-p",
        "--output-format=stream-json",
        "--verbose",
        // "--dangerously-skip-permissions",
        &format!("--settings={settings}"),
        &format!("--mcp-config={mcp_config}"),
        "--permission-prompt-tool=mcp__but-security__approval_prompt",
        "--permission-mode=acceptEdits",
    ]);
    if create_new {
        command.arg(format!("--session-id={}", session.id));
    } else {
        command.arg(format!("--resume={}", session.current_id));
    }
    command.arg(message);
    Ok(command.spawn()?)
}

/// If a session exists, it just returns it, otherwise it creates a new session
/// and makes a cooresponding rule
async fn upsert_session(
    ctx: Arc<Mutex<CommandContext>>,
    session_id: uuid::Uuid,
    stack_id: StackId,
) -> Result<crate::ClaudeSession> {
    let mut ctx = ctx.lock().await;
    let session = if let Some(session) = db::get_session_by_id(&mut ctx, session_id)? {
        session
    } else {
        let session = db::save_new_session(&mut ctx, session_id)?;
        create_claude_assignment_rule(&mut ctx, session_id, stack_id)?;
        session
    };
    Ok(session)
}

/// Spawns the thread that manages reading the CC stdout and saves the events to
/// the db and streams them to the client.
fn spawn_response_streaming(
    ctx: Arc<Mutex<CommandContext>>,
    broadcaster: Arc<Mutex<Broadcaster>>,
    read_stdout: PipeReader,
    session_id: uuid::Uuid,
    stack_id: StackId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let reader = BufReader::new(read_stdout);
        let mut first = true;
        for line in reader.lines() {
            let mut ctx = ctx.lock().await;
            let line = line.unwrap();
            let parsed_event: serde_json::Value = serde_json::from_str(&line).unwrap();

            if first {
                let current_session_id = parsed_event["session_id"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                let session = db::get_session_by_id(&mut ctx, session_id).unwrap();
                if session.is_some() {
                    db::set_session_current_id(&mut ctx, session_id, current_session_id).unwrap();
                }
                first = false;
            }

            let message_content = ClaudeMessageContent::ClaudeOutput(parsed_event.clone());
            send_claude_message(
                &mut ctx,
                broadcaster.clone(),
                session_id,
                stack_id,
                message_content,
            )
            .await
            .unwrap();
        }
    })
}

impl Default for Claudes {
    fn default() -> Self {
        Self::new()
    }
}

async fn send_claude_message(
    ctx: &mut CommandContext,
    broadcaster: Arc<Mutex<Broadcaster>>,
    session_id: uuid::Uuid,
    stack_id: StackId,
    content: ClaudeMessageContent,
) -> Result<()> {
    let message = db::save_new_message(ctx, session_id, content.clone())?;
    let project_id = ctx.project().id;

    broadcaster.lock().await.send(FrontendEvent {
        name: format!("project://{project_id}/claude/{stack_id}/message_recieved"),
        payload: json!(message),
    });
    Ok(())
}

/// Check if Claude Code is available by running the version command.
/// Returns true if the command executes successfully, false otherwise.
pub async fn check_claude_available(claude_executable: &str) -> bool {
    match Command::new(claude_executable)
        .arg("--version")
        .output()
        .await
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}
