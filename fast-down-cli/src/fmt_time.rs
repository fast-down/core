pub fn format_time(time: u64) -> String {
    let seconds = time % 60;
    let minutes = (time / 60) % 60;
    let hours = time / 3600;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}
