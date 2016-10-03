#![allow(block_in_if_condition_stmt)]  // impl_rdp! uses this

use slog::{Record, Serialize, Serializer};
use slog::ser::Error as SlogSerError;

use std::convert::From;
use std::str::FromStr;


#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Command {
    Nick,
    User,
    Join,
    Part,
    Quit,
    Ping,
    Mode,
    Pong,
    Pass,
    Privmsg,
    Topic,
    Numeric { code: u16, string: String },
    Unknown(String),
}

impl<'a> From<&'a str> for Command {
    fn from(cmd: &'a str) -> Command {
        match cmd {
            "NICK" => Command::Nick,
            "USER" => Command::User,
            "JOIN" => Command::Join,
            "PART" => Command::Part,
            "QUIT" => Command::Quit,
            "PING" => Command::Ping,
            "PONG" => Command::Pong,
            "MODE" => Command::Mode,
            "PASS" => Command::Pass,
            "PRIVMSG" => Command::Privmsg,
            "TOPIC" => Command::Topic,
            d => {
                if cmd.len() == 3 {
                    if let Ok(c) = cmd.parse() {
                        Command::Numeric {
                            code: c,
                            string: d.into(),
                        }
                    } else {
                        Command::Unknown(d.into())
                    }
                } else {
                    Command::Unknown(d.into())
                }
            }
        }
    }
}

impl From<String> for Command {
    fn from(cmd: String) -> Command {
        match cmd.as_ref() {
            "NICK" => return Command::Nick,
            "USER" => return Command::User,
            "JOIN" => return Command::Join,
            "PART" => return Command::Part,
            "QUIT" => return Command::Quit,
            "PING" => return Command::Ping,
            "PONG" => return Command::Pong,
            "MODE" => return Command::Mode,
            "PASS" => return Command::Pass,
            "PRIVMSG" => return Command::Privmsg,
            "TOPIC" => return Command::Topic,
            _ => {}
        }

        if cmd.len() == 3 {
            if let Ok(c) = cmd.parse() {
                Command::Numeric {
                    code: c,
                    string: cmd,
                }
            } else {
                Command::Unknown(cmd)
            }
        } else {
            Command::Unknown(cmd)
        }
    }
}

impl Command {
    pub fn to_str(&self) -> &str {
        match *self {
            Command::Nick => "NICK",
            Command::User => "USER",
            Command::Join => "JOIN",
            Command::Part => "PART",
            Command::Quit => "QUIT",
            Command::Ping => "PING",
            Command::Mode => "MODE",
            Command::Pong => "PONG",
            Command::Pass => "PASS",
            Command::Privmsg => "PRIVMSG",
            Command::Topic => "TOPIC",
            Command::Numeric { ref string, .. } => string,
            Command::Unknown(ref s) => s,
        }
    }
}

impl Serialize for Command {
    fn serialize(
        &self,
        _record: &Record,
        key: &str,
        serializer: &mut Serializer
    ) -> Result<(), SlogSerError> {
        serializer.emit_str(key, self.to_str())
    }
}


use pest::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IrcLine {
    pub prefix: Option<String>,
    pub command: Command,
    pub args: Vec<String>,
}

impl FromStr for IrcLine {
    type Err = ();
    fn from_str(line: &str) -> Result<IrcLine, ()> {
        parse_irc_line(line).ok_or(())
    }
}

#[derive(Default)]
struct IrcLineBuilder {
    prefix: Option<String>,
    command: Option<String>,
    args: Vec<String>,
}

impl_rdp! {
    grammar! {
        expression = _{ ([":"] ~ prefix ~ space )? ~ command ~ params }
        prefix = { ([":"] | nospcrlfcl)+ }
        command = { (['A'..'Z']+) | (digit ~ digit ~ digit) }
        digit = _{ ['0' .. '9'] }
        space = _{ [" "]+ }
        params = _{ (space ~ param)* ~ ( space ~ [":"] ~ trailing_param )?  }
        nospcrlfcl = _{ ['\x01'..'\x09'] | ['\x0B'..'\x0C'] | ['\x0E' .. '\x1F'] | ['\x21'..'\x39'] | ['\x3B'..'\x7F'] }
        param = { nospcrlfcl ~ ( [":"] | nospcrlfcl )* }
        trailing_param = { ([":"] | [" "] | nospcrlfcl)* }
    }
}


pub fn parse_irc_line(line: &str) -> Option<IrcLine> {
    let mut parser = Rdp::new(StringInput::new(line));

    if !parser.expression() || !parser.end() {
        return None;
    }

    let builder = parser.queue().iter().fold(IrcLineBuilder::default(), |mut builder, token| {
        match token.rule {
            Rule::prefix => builder.prefix = Some(line[token.start..token.end].into()),
            Rule::command => builder.command = Some(line[token.start..token.end].into()),
            Rule::param |
            Rule::trailing_param => builder.args.push(line[token.start..token.end].into()),
            Rule::any | Rule::soi | Rule::eoi => {}
        };

        builder
    });

    let IrcLineBuilder { prefix, command, args } = builder;

    Some(IrcLine {
        prefix: prefix,
        command: command.expect("expected command").into(),
        args: args,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_nick() {
        assert_eq!(parse_irc_line("NICK test"),
                   Some(IrcLine {
                       prefix: None,
                       command: Command::Nick,
                       args: vec!["test".into()],
                   }))
    }

    #[test]
    fn simple_user() {
        assert_eq!(parse_irc_line("USER test * * :Real Name"),
                   Some(IrcLine {
                       prefix: None,
                       command: Command::User,
                       args: vec!["test".into(), "*".into(), "*".into(), "Real Name".into()],
                   }))
    }

    #[test]
    fn simple_prefix() {
        assert_eq!(parse_irc_line(":example.com PRIVMSG #test :Some text"),
                   Some(IrcLine {
                       prefix: Some("example.com".into()),
                       command: Command::Privmsg,
                       args: vec!["#test".into(), "Some text".into()],
                   }))
    }

    #[test]
    fn simple_numeric() {
        assert_eq!(parse_irc_line("001 test :Some text"),
                   Some(IrcLine {
                       prefix: None,
                       command: Command::Numeric {
                           code: 1,
                           string: "001".into(),
                       },
                       args: vec!["test".into(), "Some text".into()],
                   }))
    }
}
