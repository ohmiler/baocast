//! milercast.exe: the MilerCast window. The command-line version is milercast-cli.exe.

#![windows_subsystem = "windows"]

mod gui;

fn main() {
    gui::run();
}
