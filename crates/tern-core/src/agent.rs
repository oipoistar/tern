//! Agent forwarding: a server reaches the SSH agent on this computer through
//! channels it opens back to us, which Tern pipes to the agent, but only on
//! connections where the user asked for it.

use russh::client;
use tokio::io::{AsyncRead, AsyncWrite};

/// A byte stream to the agent, whichever kind it is.
trait AgentStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AgentStream for T {}

/// The agent on this computer: the one `SSH_AUTH_SOCK` names (a socket on
/// Unix, a pipe on Windows), else on Windows OpenSSH's agent pipe, then
/// Pageant.
async fn connect() -> std::io::Result<Box<dyn AgentStream>> {
    #[cfg(unix)]
    {
        let path = std::env::var_os("SSH_AUTH_SOCK")
            .ok_or_else(|| std::io::Error::other("no SSH agent"))?;
        Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
    }
    #[cfg(windows)]
    {
        let pipe =
            std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".into());
        if let Ok(client) = tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe) {
            return Ok(Box::new(client));
        }
        let pageant = pageant::PageantStream::new()
            .await
            .map_err(std::io::Error::other)?;
        Ok(Box::new(pageant))
    }
}

/// Answers a channel the server opened to reach the agent: piped to the
/// agent while forwarding is on for this connection, refused otherwise (or
/// when there's no agent to reach).
pub(crate) fn serve(
    forwarding: bool,
    channel: russh::Channel<client::Msg>,
    reply: client::ChannelOpenHandle,
) {
    if !forwarding {
        // Dropping `reply` refuses the channel: a server can't reach the
        // agent unless it was offered.
        return;
    }
    tokio::spawn(async move {
        match connect().await {
            Ok(mut agent) => {
                reply.accept().await;
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut agent, &mut stream).await;
            }
            Err(_) => drop(reply),
        }
    });
}
