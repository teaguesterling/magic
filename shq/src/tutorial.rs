//! Interactive tutorial for learning shq features.

use std::io::{self, Write};
use std::process::Command;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    style::{Color, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
    ExecutableCommand,
};

/// A single lesson in the tutorial.
pub struct Lesson {
    pub name: &'static str,
    pub title: &'static str,
    pub steps: Vec<Step>,
}

/// A step within a lesson.
pub enum Step {
    /// Display explanatory text.
    Explain(&'static str),
    /// Run a command and show its output.
    RunCommand(&'static str),
    /// Wait for user to press Enter to continue.
    Prompt(&'static str),
    /// Clear the screen.
    Clear,
}

/// Get all available lessons.
pub fn lessons() -> Vec<Lesson> {
    vec![
        lesson_getting_started(),
        lesson_capturing_commands(),
        lesson_viewing_output(),
        lesson_working_with_errors(),
        lesson_query_syntax(),
        lesson_shell_integration(),
    ]
}

fn lesson_getting_started() -> Lesson {
    Lesson {
        name: "getting-started",
        title: "Getting Started",
        steps: vec![
            Step::Clear,
            Step::Explain(
                "Welcome to the shq tutorial!\n\n\
                 shq (Shell Query) captures your command history along with their outputs,\n\
                 exit codes, and timing information. This lets you:\n\n\
                 - Review what happened in past commands\n\
                 - Search through command outputs\n\
                 - Find and re-run previous commands\n\
                 - Track errors and warnings from builds",
            ),
            Step::Prompt("Press Enter to continue..."),
            Step::Explain(
                "First, let's check if shq is initialized and see the current state.",
            ),
            Step::RunCommand("shq stats"),
            Step::Prompt("Press Enter to continue to the next lesson..."),
        ],
    }
}

fn lesson_capturing_commands() -> Lesson {
    Lesson {
        name: "capturing-commands",
        title: "Capturing Commands",
        steps: vec![
            Step::Clear,
            Step::Explain(
                "Lesson 2: Capturing Commands\n\n\
                 The main way to capture commands is with `shq run`. This runs your command\n\
                 while capturing its output, exit code, and duration.\n\n\
                 Let's try it!",
            ),
            Step::Prompt("Press Enter to run: shq run echo \"Hello from shq tutorial!\""),
            Step::RunCommand("shq run echo \"Hello from shq tutorial!\""),
            Step::Explain(
                "\nThat command was captured! Now let's run something with more output.",
            ),
            Step::Prompt("Press Enter to run: shq run ls -la"),
            Step::RunCommand("shq run ls -la"),
            Step::Explain(
                "\nBoth commands are now stored in shq's history. Let's see them:",
            ),
            Step::RunCommand("shq history ~3"),
            Step::Prompt("Press Enter to continue to the next lesson..."),
        ],
    }
}

fn lesson_viewing_output() -> Lesson {
    Lesson {
        name: "viewing-output",
        title: "Viewing Output",
        steps: vec![
            Step::Clear,
            Step::Explain(
                "Lesson 3: Viewing Output\n\n\
                 One of shq's most useful features is retrieving output from past commands.\n\
                 Use `shq output` (or `shq o`) to view captured output.",
            ),
            Step::Explain(
                "\nThe ~N syntax means \"Nth most recent command\":\n\
                 - ~1 = most recent (the default)\n\
                 - ~2 = second most recent\n\
                 - etc.",
            ),
            Step::Prompt("Press Enter to view the last command's output: shq output ~1"),
            Step::RunCommand("shq output ~1"),
            Step::Explain(
                "\nYou can also see detailed metadata about a command with `shq info`:",
            ),
            Step::Prompt("Press Enter to run: shq info ~1"),
            Step::RunCommand("shq info ~1"),
            Step::Prompt("Press Enter to continue to the next lesson..."),
        ],
    }
}

fn lesson_working_with_errors() -> Lesson {
    Lesson {
        name: "working-with-errors",
        title: "Working with Errors",
        steps: vec![
            Step::Clear,
            Step::Explain(
                "Lesson 4: Working with Errors\n\n\
                 shq automatically parses errors and warnings from command output.\n\
                 Let's run a command that fails:",
            ),
            Step::Prompt("Press Enter to run a failing command..."),
            Step::RunCommand("shq run sh -c 'echo \"error: something went wrong\" >&2; exit 1'"),
            Step::Explain(
                "\nNotice the non-zero exit code was captured. Now let's view parsed events:",
            ),
            Step::RunCommand("shq events ~1"),
            Step::Explain(
                "\nYou can also filter history to show only failed commands using %failed:",
            ),
            Step::RunCommand("shq history %failed~5"),
            Step::Prompt("Press Enter to continue to the next lesson..."),
        ],
    }
}

fn lesson_query_syntax() -> Lesson {
    Lesson {
        name: "query-syntax",
        title: "Query Syntax",
        steps: vec![
            Step::Clear,
            Step::Explain(
                "Lesson 5: Query Syntax\n\n\
                 shq uses a concise query syntax for selecting commands:\n\n\
                 Range syntax:\n\
                 - ~N      = Nth most recent (e.g., ~1 = last, ~5 = 5th from end)\n\
                 - ~N:     = Last N commands (e.g., ~10: = last 10)\n\
                 - ~10:~5  = Range from 10th to 5th most recent\n\n\
                 Filters (prefix with %):\n\
                 - %failed    = Non-zero exit code\n\
                 - %ok        = Zero exit code\n\
                 - cmd~/pat/  = Command matches pattern",
            ),
            Step::Prompt("Press Enter to see examples..."),
            Step::Explain("\nShow last 5 commands:"),
            Step::RunCommand("shq history ~5:"),
            Step::Explain("\nFilter commands containing 'echo':"),
            Step::RunCommand("shq history cmd~/echo/~10:"),
            Step::Prompt("Press Enter to continue to the next lesson..."),
        ],
    }
}

fn lesson_shell_integration() -> Lesson {
    Lesson {
        name: "shell-integration",
        title: "Shell Integration",
        steps: vec![
            Step::Clear,
            Step::Explain(
                "Lesson 6: Shell Integration\n\n\
                 For seamless capture of all your commands, shq can integrate with your shell.\n\
                 This automatically captures every command you run!\n\n\
                 To set it up, add this to your shell config (.zshrc or .bashrc):\n\n\
                     eval \"$(shq hook init)\"\n\n\
                 Then use `shq-on` to start capturing and `shq-off` to stop.",
            ),
            Step::Explain(
                "\nLet's see what the hook code looks like:",
            ),
            Step::Prompt("Press Enter to run: shq hook init"),
            Step::RunCommand("shq hook init --quiet"),
            Step::Clear,
            Step::Explain(
                "Congratulations! You've completed the shq tutorial.\n\n\
                 Quick reference:\n\
                 - shq run <cmd>     Run and capture a command\n\
                 - shq history       List recent commands\n\
                 - shq output ~N     View output of Nth recent command\n\
                 - shq info ~N       Show metadata for a command\n\
                 - shq events        View parsed errors/warnings\n\
                 - shq rerun ~N      Re-run a previous command\n\n\
                 For more help, run: shq quick-help\n\n\
                 Happy querying!",
            ),
            Step::Prompt("Press Enter to exit the tutorial..."),
        ],
    }
}

/// List available lessons.
pub fn list_lessons() {
    println!("Available lessons:\n");
    for (i, lesson) in lessons().iter().enumerate() {
        println!("  {}. {} ({})", i + 1, lesson.title, lesson.name);
    }
    println!("\nRun a specific lesson with: shq tutorial --lesson <number or name>");
}

/// Run the tutorial, optionally starting at a specific lesson.
pub fn run_tutorial(lesson_selector: Option<&str>) -> bird::Result<()> {
    let all_lessons = lessons();

    // Determine which lessons to run
    let lessons_to_run: Vec<&Lesson> = if let Some(selector) = lesson_selector {
        // Try to parse as number first
        if let Ok(num) = selector.parse::<usize>() {
            if num == 0 || num > all_lessons.len() {
                eprintln!(
                    "Lesson {} not found. Use --list to see available lessons.",
                    num
                );
                return Ok(());
            }
            vec![&all_lessons[num - 1]]
        } else {
            // Try to match by name
            let matching: Vec<_> = all_lessons
                .iter()
                .filter(|l| l.name == selector || l.title.to_lowercase().contains(&selector.to_lowercase()))
                .collect();
            if matching.is_empty() {
                eprintln!(
                    "Lesson '{}' not found. Use --list to see available lessons.",
                    selector
                );
                return Ok(());
            }
            matching
        }
    } else {
        // Run all lessons
        all_lessons.iter().collect()
    };

    // Enable raw mode for keypress detection
    terminal::enable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;

    let result = run_lessons(&lessons_to_run);

    // Always restore terminal
    let _ = terminal::disable_raw_mode();
    println!(); // Ensure we're on a new line

    result
}

fn run_lessons(lessons: &[&Lesson]) -> bird::Result<()> {
    let mut stdout = io::stdout();

    for (lesson_idx, lesson) in lessons.iter().enumerate() {
        // Show lesson header
        stdout.execute(SetForegroundColor(Color::Cyan))?;
        print!("\n=== Lesson {}: {} ===\n", lesson_idx + 1, lesson.title);
        stdout.execute(ResetColor)?;
        stdout.flush()?;

        for step in &lesson.steps {
            if !execute_step(&mut stdout, step)? {
                // User pressed Ctrl+C
                return Ok(());
            }
        }
    }

    Ok(())
}

fn execute_step(stdout: &mut io::Stdout, step: &Step) -> bird::Result<bool> {
    match step {
        Step::Clear => {
            stdout.execute(terminal::Clear(ClearType::All))?;
            stdout.execute(crossterm::cursor::MoveTo(0, 0))?;
        }
        Step::Explain(text) => {
            // Temporarily disable raw mode for proper text display
            terminal::disable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
            stdout.execute(SetForegroundColor(Color::White))?;
            println!("\n{}", text);
            stdout.execute(ResetColor)?;
            stdout.flush()?;
            terminal::enable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
        }
        Step::RunCommand(cmd) => {
            // Show the command being run
            terminal::disable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
            stdout.execute(SetForegroundColor(Color::Yellow))?;
            println!("\n$ {}", cmd);
            stdout.execute(ResetColor)?;
            stdout.flush()?;

            // Run the command
            let output = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .map_err(bird::Error::Io)?;

            // Display output
            stdout.execute(SetForegroundColor(Color::DarkGrey))?;
            if !output.stdout.is_empty() {
                print!("{}", String::from_utf8_lossy(&output.stdout));
            }
            if !output.stderr.is_empty() {
                stdout.execute(SetForegroundColor(Color::Red))?;
                print!("{}", String::from_utf8_lossy(&output.stderr));
            }
            stdout.execute(ResetColor)?;
            stdout.flush()?;

            terminal::enable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
        }
        Step::Prompt(text) => {
            // Show prompt and wait for keypress
            terminal::disable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
            stdout.execute(SetForegroundColor(Color::Green))?;
            print!("\n{}", text);
            stdout.execute(ResetColor)?;
            stdout.flush()?;
            terminal::enable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;

            // Wait for Enter or Ctrl+C
            loop {
                if event::poll(std::time::Duration::from_millis(100))
                    .map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?
                {
                    if let Event::Key(key_event) = event::read()
                        .map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?
                    {
                        match key_event.code {
                            KeyCode::Enter => break,
                            KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                                terminal::disable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
                                println!("\n\nTutorial interrupted. Run 'shq tutorial' to continue later.");
                                return Ok(false);
                            }
                            KeyCode::Char('q') => {
                                terminal::disable_raw_mode().map_err(|e| bird::Error::Io(io::Error::new(io::ErrorKind::Other, e)))?;
                                println!("\n\nTutorial ended. Run 'shq tutorial' to start again.");
                                return Ok(false);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    Ok(true)
}
