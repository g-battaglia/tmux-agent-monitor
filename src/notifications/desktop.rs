//! Desktop commands are fixed programs with data-only argv, never shell code.
use super::DesktopNotification;
use crate::paths;
use anyhow::{Result, ensure};
use std::process::Command;

pub trait DesktopBackend: Send {
    fn label(&self) -> String;
    fn send(&mut self, notification: &DesktopNotification) -> Result<()>;
}

#[derive(Clone, Copy)]
enum Kind {
    MacOs,
    Linux,
}

pub struct SystemDesktop;
impl DesktopBackend for SystemDesktop {
    fn label(&self) -> String {
        if cfg!(target_os = "macos") {
            "macOS (osascript; desktop permission required)".into()
        } else {
            let installed = std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join("notify-send").is_file())
            });
            if installed {
                "Linux (notify-send)"
            } else {
                "Linux (notify-send not installed)"
            }
            .into()
        }
    }
    fn send(&mut self, notification: &DesktopNotification) -> Result<()> {
        let kind = if cfg!(target_os = "macos") {
            Kind::MacOs
        } else {
            Kind::Linux
        };
        let output = paths::output(&mut command(kind, notification))?;
        // Do not echo helper output: it could contain sensitive desktop details.
        ensure!(
            output.status.success(),
            "desktop helper rejected notification (check desktop permissions/session)"
        );
        Ok(())
    }
}

const APPLESCRIPT: &str = "on run argv\nset noticeTitle to item 1 of argv\nset noticeBody to item 2 of argv\ndisplay notification noticeBody with title noticeTitle\nend run";
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn command(kind: Kind, notification: &DesktopNotification) -> Command {
    match kind {
        Kind::MacOs => {
            let mut command = Command::new("/usr/bin/osascript");
            command.args([
                "-e",
                APPLESCRIPT,
                "--",
                &notification.title,
                &notification.body,
            ]);
            command
        }
        Kind::Linux => {
            let mut command = Command::new("notify-send");
            command.args([
                "--app-name=tmux-agent-monitor",
                "--",
                &escape_markup(&notification.title),
                &escape_markup(&notification.body),
            ]);
            command
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_text_remains_data_on_both_platforms() {
        let notice = DesktopNotification {
            title: "-title \" & <b>".into(),
            body: "\"\nend run\ndo shell script \"touch /tmp/not-executed\" & <b>".into(),
        };
        let mac = command(Kind::MacOs, &notice);
        let args = mac
            .get_args()
            .map(|s| s.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(args, ["-e", APPLESCRIPT, "--", &notice.title, &notice.body]);
        assert!(!APPLESCRIPT.contains("touch"));
        let linux = command(Kind::Linux, &notice);
        let args = linux
            .get_args()
            .map(|s| s.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(args[1], "--");
        assert_eq!(args[2], "-title \" &amp; &lt;b&gt;");
        assert!(!args[3].contains("<b>"));
    }
}
