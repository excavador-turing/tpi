// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::{stdout, Write};

use anyhow::{bail, Error, Result};
use crossterm::cursor::MoveToColumn;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};

/// Ctrl+C. Not a line the caller should treat as Enter.
#[derive(Debug)]
struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

struct Prompt {
    msg: &'static str,
    password: bool,
    /// Characters, so the cursor is a character index. `String::insert` and
    /// `remove` index bytes, and a non-ASCII character makes those disagree.
    input: Vec<char>,
    cursor_idx: usize,
}

impl Prompt {
    fn new(msg: &'static str, password: bool) -> Self {
        Self {
            msg,
            password,
            input: Vec::new(),
            cursor_idx: 0,
        }
    }

    fn text(&self) -> String {
        self.input.iter().collect()
    }

    fn read(&mut self) -> Result<String> {
        enable_raw_mode()?;

        let res = self.read_loop();

        let _ = disable_raw_mode();

        match res {
            Ok(()) => Ok(self.text()),
            Err(e) if e.is::<Interrupted>() => Err(e),
            Err(e) => bail!("failed to get terminal event: {e}"),
        }
    }

    fn read_loop(&mut self) -> Result<()> {
        loop {
            self.print()?;

            let cont = match event::read()? {
                Event::Key(key) => self.handle_key(key)?,
                _ => true,
            };

            if !cont {
                break;
            }
        }

        Ok(())
    }

    fn print(&self) -> Result<()> {
        queue!(
            stdout(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(format!("{}: ", self.msg)),
        )?;

        if !self.password {
            let column = self.msg.len() + self.cursor_idx + 2;
            let column = u16::try_from(column).unwrap_or(0);

            queue!(stdout(), Print(self.text()), MoveToColumn(column))?;
        }

        stdout().flush()?;

        Ok(())
    }

    fn handle_key(&mut self, key: event::KeyEvent) -> Result<bool> {
        match decide(&key) {
            LineEnd::Abort => {
                execute!(stdout(), Print("\n\r"))?;
                return Err(Error::new(Interrupted));
            }
            LineEnd::Submit => {
                execute!(stdout(), Print("\n\r"))?;
                return Ok(false);
            }
            LineEnd::Edit => {}
        }

        match key.code {
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Delete => {
                if !self.input.is_empty() {
                    self.delete();
                }
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left => self.left(),
            KeyCode::Right if self.can_move_right() => {
                self.cursor_idx += 1;
            }
            _ => {}
        }

        Ok(true)
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_idx, c);
        self.cursor_idx += 1;
    }

    fn backspace(&mut self) {
        if !self.input.is_empty() {
            self.left();
            self.delete();
        }
    }

    /// Whether the cursor has somewhere to the right to go.
    ///
    /// This was `cursor_idx < input.len() - 1`, which underflows on an
    /// empty prompt: `0usize - 1` panics in debug and wraps to usize::MAX
    /// in release, so Right moved the cursor past the end and the next
    /// character typed panicked inside `String::insert`. Adding to the left
    /// side instead of subtracting from the right keeps the same answer for
    /// every non-empty input and is defined for the empty one.
    fn can_move_right(&self) -> bool {
        self.cursor_idx + 1 < self.input.len()
    }

    fn left(&mut self) {
        if self.cursor_idx > 0 {
            self.cursor_idx -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor_idx < self.input.len() {
            self.input.remove(self.cursor_idx);
        }
    }
}

pub fn simple(msg: &'static str) -> Result<String> {
    Prompt::new(msg, false).read()
}

pub fn password(msg: &'static str) -> Result<String> {
    Prompt::new(msg, true).read()
}

/// What a key does to the line, before any character is edited.
enum LineEnd {
    Edit,
    Submit,
    Abort,
}

fn decide(key: &event::KeyEvent) -> LineEnd {
    let interrupt = key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c');
    if interrupt {
        LineEnd::Abort
    } else if key.code == KeyCode::Enter {
        LineEnd::Submit
    } else {
        LineEnd::Edit
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, LineEnd, Prompt};
    use crossterm::event::{self, KeyCode, KeyModifiers};

    fn at(input: &str, cursor_idx: usize) -> Prompt {
        Prompt {
            msg: "",
            password: false,
            input: input.chars().collect(),
            cursor_idx,
        }
    }

    /// The bug: `len() - 1` on an empty String. In debug this panicked; in
    /// release it wrapped to usize::MAX, every comparison passed, and the
    /// cursor walked off the end until `String::insert` panicked instead.
    #[test]
    fn an_empty_prompt_has_nowhere_to_move_right() {
        assert!(!at("", 0).can_move_right());
    }

    #[test]
    fn a_single_character_has_nowhere_to_move_right() {
        assert!(!at("x", 0).can_move_right());
    }

    /// Everything non-empty must answer exactly as `cursor_idx < len - 1`
    /// did, or this is a behaviour change wearing a bug fix's clothes.
    #[test]
    fn non_empty_input_answers_as_the_old_expression_did() {
        for text in ["a", "ab", "abcdef"] {
            for cursor_idx in 0..=text.len() {
                let old = cursor_idx < text.len() - 1;
                assert_eq!(
                    at(text, cursor_idx).can_move_right(),
                    old,
                    "{text:?} at {cursor_idx}"
                );
            }
        }
    }

    #[test]
    fn a_non_ascii_character_is_one_character_for_the_next_key() {
        let mut prompt = at("", 0);
        prompt.insert_char('ä');
        prompt.insert_char('b');
        assert_eq!(prompt.text(), "äb");
    }

    #[test]
    fn backspace_removes_the_whole_non_ascii_character() {
        let mut prompt = at("", 0);
        prompt.insert_char('ä');
        prompt.backspace();
        assert_eq!(prompt.text(), "");
        assert_eq!(prompt.cursor_idx, 0);
    }

    #[test]
    fn control_c_aborts_and_enter_submits() {
        let abort = event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let submit = event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(decide(&abort), LineEnd::Abort));
        assert!(matches!(decide(&submit), LineEnd::Submit));
    }
}
