use crossterm::{
    cursor,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal, ExecutableCommand,
};
use std::io::{self, Write};
use std::time::Instant;

const BAR: Color = Color::AnsiValue(237);
const LABEL: Color = Color::AnsiValue(250);
const DIM: Color = Color::AnsiValue(242);

struct Spinner {
    name: &'static str,
    frames: &'static [&'static str],
}

const SPINNERS: &[Spinner] = &[
    Spinner { name: "pipe",         frames: &["|", "/", "-", "\\"] },
    Spinner { name: "braille-race", frames: &["⠁", "⠉", "⠙", "⠸", "⢰", "⣠", "⣄", "⡆", "⠇", "⠃"] },
    Spinner { name: "bolt-ascii",   frames: &["*", "+", "·", "+"] },
    Spinner { name: "florette",     frames: &["✿", "❀", "✾", "❁"] },
];

fn format_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else {
        format!("{}m{}s", s / 60, s % 60)
    }
}

fn draw(out: &mut io::Stdout, scroll: usize, start: Instant, width: usize, height: usize) -> io::Result<()> {
    let elapsed = start.elapsed();
    let time_str = format_elapsed(elapsed);
    let visible_rows = height.saturating_sub(1); // reserve bottom row for hints

    for row in 0..visible_rows {
        let spinner_idx = (row + scroll) / 2;
        let is_label = (row + scroll) % 2 == 1;

        out.execute(cursor::MoveTo(0, row as u16))?;
        out.execute(terminal::Clear(terminal::ClearType::CurrentLine))?;

        if spinner_idx >= SPINNERS.len() {
            continue;
        }
        let spinner = &SPINNERS[spinner_idx];

        if is_label {
            out.execute(SetForegroundColor(DIM))?;
            out.execute(Print(format!("  {:16}", spinner.name)))?;
            out.execute(SetForegroundColor(LABEL))?;
            out.execute(Print(format!("frames: {}", spinner.frames.join(" "))))?;
            out.execute(ResetColor)?;
        } else {
            let frame_idx = (elapsed.as_millis() / 120) as usize % spinner.frames.len();
            let frame = spinner.frames[frame_idx];
            let tail = format!(" {} {} ─", frame, time_str);
            let tail_chars: usize = tail.chars().count();
            let bar_len = width.saturating_sub(tail_chars);

            out.execute(SetForegroundColor(BAR))?;
            out.execute(Print("─".repeat(bar_len)))?;
            out.execute(ResetColor)?;
            out.execute(SetAttribute(Attribute::Dim))?;
            out.execute(Print(format!(" {} {} ", frame, time_str)))?;
            out.execute(SetAttribute(Attribute::Reset))?;
            out.execute(SetForegroundColor(BAR))?;
            out.execute(Print("─"))?;
            out.execute(ResetColor)?;
        }
    }

    // Status bar
    out.execute(cursor::MoveTo(0, visible_rows as u16))?;
    out.execute(terminal::Clear(terminal::ClearType::CurrentLine))?;
    out.execute(SetAttribute(Attribute::Dim))?;
    let current = scroll / 2 + 1;
    let total = SPINNERS.len();
    out.execute(Print(format!(" j/k scroll · q quit · {}/{}", current, total)))?;
    out.execute(SetAttribute(Attribute::Reset))?;

    out.flush()?;
    Ok(())
}

fn main() -> io::Result<()> {
    let mut out = io::stdout();

    terminal::enable_raw_mode()?;
    out.execute(terminal::EnterAlternateScreen)?;
    out.execute(cursor::Hide)?;

    let start = Instant::now();
    let mut scroll: usize = 0;
    let max_scroll = (SPINNERS.len() * 2).saturating_sub(1);

    loop {
        let (cols, rows) = terminal::size()?;
        let width = cols as usize;
        let height = rows as usize;

        draw(&mut out, scroll, start, width, height)?;
        std::thread::sleep(std::time::Duration::from_millis(60));

        if crossterm::event::poll(std::time::Duration::from_millis(0))? {
            use crossterm::event::{Event, KeyCode, KeyEvent};
            if let Event::Key(KeyEvent { code, .. }) = crossterm::event::read()? {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        scroll = (scroll + 2).min(max_scroll);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        scroll = scroll.saturating_sub(2);
                    }
                    KeyCode::Char('d') => {
                        scroll = (scroll + 10).min(max_scroll);
                    }
                    KeyCode::Char('u') => {
                        scroll = scroll.saturating_sub(10);
                    }
                    KeyCode::Char('g') => scroll = 0,
                    KeyCode::Char('G') => scroll = max_scroll,
                    _ => {}
                }
            }
        }
    }

    out.execute(cursor::Show)?;
    out.execute(terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    Ok(())
}
