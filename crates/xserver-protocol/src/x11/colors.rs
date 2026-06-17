use super::Rgb16;

pub fn lookup_color_name(name: &str) -> Option<Rgb16> {
    let normalized: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();

    let gray_n = normalized
        .strip_prefix("gray")
        .or_else(|| normalized.strip_prefix("grey"));
    if let Some(rest) = gray_n {
        if rest.is_empty() {
            return Some(rgb8(190, 190, 190));
        }
        if let Ok(percent) = rest.parse::<u32>() {
            let value = u8::try_from(percent.min(100) * 255 / 100).unwrap_or(u8::MAX);
            return Some(rgb8(value, value, value));
        }
    }

    let (r, g, b) = match normalized.as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" | "red1" => (255, 0, 0),
        "red2" => (238, 0, 0),
        "red3" => (205, 0, 0),
        "red4" => (139, 0, 0),
        "green" | "green1" => (0, 255, 0),
        "green2" => (0, 238, 0),
        "green3" => (0, 205, 0),
        "green4" => (0, 139, 0),
        "blue" | "blue1" => (0, 0, 255),
        "blue2" => (0, 0, 238),
        "blue3" => (0, 0, 205),
        "blue4" => (0, 0, 139),
        "yellow" | "yellow1" => (255, 255, 0),
        "yellow2" => (238, 238, 0),
        "yellow3" => (205, 205, 0),
        "yellow4" => (139, 139, 0),
        "cyan" | "cyan1" => (0, 255, 255),
        "cyan2" => (0, 238, 238),
        "cyan3" => (0, 205, 205),
        "cyan4" => (0, 139, 139),
        "magenta" | "magenta1" => (255, 0, 255),
        "magenta2" => (238, 0, 238),
        "magenta3" => (205, 0, 205),
        "magenta4" => (139, 0, 139),
        "orange" => (255, 165, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        "purple" => (160, 32, 240),
        "navy" | "navyblue" => (0, 0, 128),
        "gold" => (255, 215, 0),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "darkgray" | "darkgrey" => (169, 169, 169),
        _ => return None,
    };

    Some(rgb8(r, g, b))
}

fn rgb8(r: u8, g: u8, b: u8) -> Rgb16 {
    Rgb16 {
        red: u16::from(r) * 257,
        green: u16::from(g) * 257,
        blue: u16::from(b) * 257,
    }
}
