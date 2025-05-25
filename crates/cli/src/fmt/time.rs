pub fn format(time: u64) -> String {
    let seconds = time % 60;
    let minutes = (time / 60) % 60;
    let hours = time / 3600;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time() {
        assert_eq!(format(0), "00:00:00");
        assert_eq!(format(59), "00:00:59");
        assert_eq!(format(60), "00:01:00");
        assert_eq!(format(3599), "00:59:59");
        assert_eq!(format(3600), "01:00:00");
        assert_eq!(format(3661), "01:01:01");
        assert_eq!(format(86399), "23:59:59");
    }
}
