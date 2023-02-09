use crate::error_msg;

pub fn opt_string_eval(value: &Option<String>) -> String {
    if let Some(value) = value {
        value.to_string()
    } else {
        String::from("-")
    }
}

pub fn invalid_machine() {
    error_msg!("Error: specify a machine name to work with. See --help for more information.");
}

pub fn invalid_image() {
    error_msg!("Error: specify a image name to work with. See --help for more information.");
}

// Macro to print success similar a println!() macro
#[macro_export]
macro_rules! success_msg {
    ($($arg:tt)*) => ({
        use termion::{color, style};
        println!("{}{}{}{}", color::Fg(color::Green), style::Bold, format_args!($($arg)*), style::Reset);
    });
}

// Macro to warn users similar a println!() macro
#[macro_export]
macro_rules! warn_msg {
    ($($arg:tt)*) => ({
        use termion::{color, style};
        println!("{}{}{}{}", color::Fg(color::Yellow), style::Bold, format_args!($($arg)*), style::Reset);
    });
}

// Macro to print errors similar a println!() macro
#[macro_export]
macro_rules! error_msg {
    ($($arg:tt)*) => ({
        use termion::{color, style};
        println!("{}{}{}{}", color::Fg(color::Red), style::Bold, format_args!($($arg)*), style::Reset);
    });
}
