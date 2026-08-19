//! Slash-command parser, shared by the terminal UI, the CLI and the web
//! dashboard so all three accept exactly the same syntax.

use std::path::PathBuf;

/// A parsed line of user input.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Plain text, or `/chat …` — broadcast to every peer.
    Broadcast(String),
    /// `/msg <peer> <text>` — direct message.
    Direct { peer: String, message: String },
    /// `/peers`
    Peers,
    /// `/alias <peer> <name>`, or `/alias <peer>` to clear.
    Alias { peer: String, alias: Option<String> },
    /// `/sendfile <peer> <path>`
    SendFile { peer: String, path: PathBuf },
    /// `/call <peer>`
    Call { peer: String },
    /// `/hangup [session]` — the only active call if omitted.
    HangUp { session: Option<String> },
    /// `/ping <peer>`
    Ping { peer: String },
    /// `/share on|off`
    Share(bool),
    /// `/me <name>` — change this node's display name.
    Rename(String),
    /// `/clear`
    Clear,
    /// `/status`
    Status,
    /// `/help`
    Help,
    /// `/quit`
    Quit,
    /// Recognised as a command but malformed; the string is a usage hint.
    Invalid(String),
}

/// Every command, for `/help` and the web dashboard's hint list.
pub const HELP: &[(&str, &str)] = &[
    ("<text>", "Broadcast a message to every peer"),
    ("/msg <peer> <text>", "Send a private message to one peer"),
    ("/peers", "List discovered peers"),
    ("/alias <peer> [name]", "Nickname a peer, or clear it"),
    ("/sendfile <peer> <path>", "Send a file"),
    ("/call <peer>", "Start a voice call"),
    ("/hangup [session]", "End a call"),
    ("/ping <peer>", "Measure round-trip time to a peer"),
    ("/share on|off", "Toggle resource sharing"),
    ("/me <name>", "Change your display name"),
    ("/status", "Show this node's status"),
    ("/clear", "Clear the message pane"),
    ("/help", "Show this list"),
    ("/quit", "Exit"),
];

/// Parse one line of input.
///
/// `<peer>` is anything [`PeerRegistry::resolve`] accepts: a node id, an id
/// prefix, a username, or an alias.
///
/// [`PeerRegistry::resolve`]: crate::net::peers::PeerRegistry::resolve
pub fn parse(line: &str) -> Option<Command> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    if !line.starts_with('/') {
        return Some(Command::Broadcast(line.to_string()));
    }

    // `//text` escapes a message that genuinely begins with a slash.
    if let Some(rest) = line.strip_prefix("//") {
        return Some(Command::Broadcast(format!("/{rest}")));
    }

    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };

    let usage = |text: &str| Some(Command::Invalid(format!("usage: {text}")));
    let require_peer = |rest: &str, text: &str| -> Result<String, Option<Command>> {
        match rest.split_whitespace().next() {
            Some(peer) => Ok(peer.to_string()),
            None => Err(usage(text)),
        }
    };

    match verb.to_ascii_lowercase().as_str() {
        "/chat" | "/say" => {
            if rest.is_empty() {
                usage("/chat <message>")
            } else {
                Some(Command::Broadcast(rest.to_string()))
            }
        }
        "/msg" | "/dm" | "/tell" => match rest.split_once(char::is_whitespace) {
            Some((peer, message)) if !message.trim().is_empty() => Some(Command::Direct {
                peer: peer.to_string(),
                message: message.trim().to_string(),
            }),
            _ => usage("/msg <peer> <message>"),
        },
        "/peers" | "/list" => Some(Command::Peers),
        "/alias" | "/nick" => {
            let mut parts = rest.splitn(2, char::is_whitespace);
            match parts.next().filter(|p| !p.is_empty()) {
                Some(peer) => Some(Command::Alias {
                    peer: peer.to_string(),
                    // No name clears the alias.
                    alias: parts
                        .next()
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .map(String::from),
                }),
                None => usage("/alias <peer> [name]"),
            }
        }
        "/sendfile" | "/send" | "/file" => match rest.split_once(char::is_whitespace) {
            // The path is the whole remainder, so filenames may contain spaces.
            Some((peer, path)) if !path.trim().is_empty() => Some(Command::SendFile {
                peer: peer.to_string(),
                path: PathBuf::from(unquote(path.trim())),
            }),
            _ => usage("/sendfile <peer> <path>"),
        },
        "/call" => match require_peer(rest, "/call <peer>") {
            Ok(peer) => Some(Command::Call { peer }),
            Err(invalid) => invalid,
        },
        "/hangup" | "/endcall" => Some(Command::HangUp {
            session: rest.split_whitespace().next().map(String::from),
        }),
        "/ping" => match require_peer(rest, "/ping <peer>") {
            Ok(peer) => Some(Command::Ping { peer }),
            Err(invalid) => invalid,
        },
        "/share" => match rest.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "enable" => Some(Command::Share(true)),
            "off" | "false" | "no" | "disable" => Some(Command::Share(false)),
            _ => usage("/share on|off"),
        },
        "/me" | "/name" => {
            if rest.is_empty() {
                usage("/me <name>")
            } else {
                Some(Command::Rename(rest.to_string()))
            }
        }
        "/clear" | "/cls" => Some(Command::Clear),
        "/status" | "/info" => Some(Command::Status),
        "/help" | "/?" => Some(Command::Help),
        "/quit" | "/exit" | "/q" => Some(Command::Quit),
        other => Some(Command::Invalid(format!(
            "unknown command \"{other}\" — try /help"
        ))),
    }
}

/// Strip one matched pair of surrounding quotes.
fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_text_broadcasts() {
        assert_eq!(
            parse("hello everyone"),
            Some(Command::Broadcast("hello everyone".into()))
        );
        assert_eq!(
            parse("  padded  "),
            Some(Command::Broadcast("padded".into()))
        );
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
    }

    #[test]
    fn a_double_slash_escapes_a_literal_slash() {
        assert_eq!(parse("//peers"), Some(Command::Broadcast("/peers".into())));
    }

    #[test]
    fn direct_messages_keep_their_full_text() {
        assert_eq!(
            parse("/msg alice hello there, how are you?"),
            Some(Command::Direct {
                peer: "alice".into(),
                message: "hello there, how are you?".into()
            })
        );
        assert!(matches!(parse("/msg alice"), Some(Command::Invalid(_))));
        assert!(matches!(parse("/msg"), Some(Command::Invalid(_))));
    }

    #[test]
    fn file_paths_may_contain_spaces_and_quotes() {
        assert_eq!(
            parse(r"/sendfile bob C:\Users\me\My Documents\report.pdf"),
            Some(Command::SendFile {
                peer: "bob".into(),
                path: PathBuf::from(r"C:\Users\me\My Documents\report.pdf")
            })
        );
        assert_eq!(
            parse(r#"/sendfile bob "/home/me/a file.txt""#),
            Some(Command::SendFile {
                peer: "bob".into(),
                path: PathBuf::from("/home/me/a file.txt")
            })
        );
        assert!(matches!(parse("/sendfile bob"), Some(Command::Invalid(_))));
    }

    #[test]
    fn alias_with_no_name_clears_it() {
        assert_eq!(
            parse("/alias 550e Studio Mac"),
            Some(Command::Alias {
                peer: "550e".into(),
                alias: Some("Studio Mac".into())
            })
        );
        assert_eq!(
            parse("/alias 550e"),
            Some(Command::Alias {
                peer: "550e".into(),
                alias: None
            })
        );
    }

    #[test]
    fn share_accepts_the_obvious_synonyms() {
        for on in ["/share on", "/share YES", "/share true", "/share enable"] {
            assert_eq!(parse(on), Some(Command::Share(true)), "{on}");
        }
        for off in ["/share off", "/share no", "/share FALSE"] {
            assert_eq!(parse(off), Some(Command::Share(false)), "{off}");
        }
        assert!(matches!(parse("/share maybe"), Some(Command::Invalid(_))));
    }

    #[test]
    fn commands_are_case_insensitive_and_have_aliases() {
        assert_eq!(parse("/PEERS"), Some(Command::Peers));
        assert_eq!(parse("/list"), Some(Command::Peers));
        assert_eq!(parse("/Quit"), Some(Command::Quit));
        assert_eq!(parse("/q"), Some(Command::Quit));
        assert_eq!(parse("/exit"), Some(Command::Quit));
    }

    #[test]
    fn hangup_takes_an_optional_session() {
        assert_eq!(parse("/hangup"), Some(Command::HangUp { session: None }));
        assert_eq!(
            parse("/hangup sess-123"),
            Some(Command::HangUp {
                session: Some("sess-123".into())
            })
        );
    }

    #[test]
    fn an_unknown_command_says_so_rather_than_being_broadcast() {
        // Broadcasting a typo'd command to the whole LAN would be worse.
        match parse("/nonsense") {
            Some(Command::Invalid(msg)) => assert!(msg.contains("/help")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn every_help_entry_parses_as_something_real() {
        for (syntax, _) in HELP {
            let Some(verb) = syntax.split_whitespace().next() else {
                continue;
            };
            if !verb.starts_with('/') {
                continue;
            }
            // Feed placeholder arguments so argument-taking commands are valid.
            let line = format!("{verb} peer-a rest of the line");
            assert!(
                !matches!(parse(&line), Some(Command::Invalid(m)) if m.contains("unknown")),
                "help lists {verb}, but the parser does not know it"
            );
        }
    }
}
