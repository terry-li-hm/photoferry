use owo_colors::OwoColorize;

pub fn print_header(text: &str) {
    println!("{}", text.bold());
}

pub fn print_success(text: &str) {
    println!("{} {}", "✓".green().bold(), text);
}

pub fn print_error(text: &str) {
    eprintln!("{} {}", "✗".red().bold(), text);
}

pub fn print_warning(text: &str) {
    println!("{} {}", "!".yellow().bold(), text);
}

pub fn print_info(text: &str) {
    println!("{} {}", "·".dimmed(), text);
}
