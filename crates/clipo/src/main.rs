// Suppress the GUI's console in release; daemon talks via tracing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    clipo::run();
}
