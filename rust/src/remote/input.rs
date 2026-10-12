//! Viewer input: the compact events a browser sends become the arguments of
//! the Player's `input` operation. Anything unknown or out of range is dropped.
use serde_json::{Value, json};

/// Events accepted from one viewer message.
pub const MAXIMUM_EVENTS: usize = 256;

fn number(event: &Value, key: &str) -> Option<f64> {
    event.get(key)?.as_f64().filter(|value| value.is_finite())
}
// A position inside the picture, both axes 0..1 from the top-left corner.
fn point(event: &Value) -> Option<Value> {
    let (x, y) = (number(event, "x")?, number(event, "y")?);
    Some(json!({"x": x.clamp(0.0, 1.0), "y": y.clamp(0.0, 1.0)}))
}

/// The Input System `Key` named by a DOM `KeyboardEvent.code`.
pub fn key_name(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key")
        && letter.len() == 1
        && letter.as_bytes()[0].is_ascii_uppercase()
    {
        return Some(letter.into());
    }
    for prefix in ["Digit", "Numpad"] {
        if let Some(digit) = code.strip_prefix(prefix)
            && digit.len() == 1
            && digit.as_bytes()[0].is_ascii_digit()
        {
            return Some(code.into());
        }
    }
    if let Some(index) = code.strip_prefix('F')
        && index
            .parse::<u8>()
            .is_ok_and(|index| (1..=12).contains(&index))
        && !index.starts_with('0')
    {
        return Some(code.into());
    }
    let name = match code {
        "Space" | "Enter" | "Tab" | "Escape" | "Backspace" | "Delete" | "Insert" | "Home"
        | "End" | "PageUp" | "PageDown" | "CapsLock" | "Comma" | "Period" | "Slash"
        | "Backslash" | "Semicolon" | "Quote" | "Backquote" | "Minus" | "ContextMenu"
        | "NumpadEnter" | "NumpadDivide" | "NumpadMultiply" | "NumLock" | "ScrollLock"
        | "Pause" | "PrintScreen" => code,
        "ArrowUp" => "UpArrow",
        "ArrowDown" => "DownArrow",
        "ArrowLeft" => "LeftArrow",
        "ArrowRight" => "RightArrow",
        "ShiftLeft" => "LeftShift",
        "ShiftRight" => "RightShift",
        "ControlLeft" => "LeftCtrl",
        "ControlRight" => "RightCtrl",
        "AltLeft" => "LeftAlt",
        "AltRight" => "RightAlt",
        "MetaLeft" => "LeftMeta",
        "MetaRight" => "RightMeta",
        "Equal" => "Equals",
        "BracketLeft" => "LeftBracket",
        "BracketRight" => "RightBracket",
        "NumpadAdd" => "NumpadPlus",
        "NumpadSubtract" => "NumpadMinus",
        "NumpadDecimal" => "NumpadPeriod",
        "NumpadEqual" => "NumpadEquals",
        _ => return None,
    };
    Some(name.into())
}

fn translate_one(event: &Value) -> Option<Value> {
    match event.get("t")?.as_str()? {
        "k" => Some(json!({
            "action": "key",
            "key": key_name(event.get("c")?.as_str()?)?,
            "pressed": event.get("down")?.as_bool()?,
        })),
        "m" => Some(json!({"action": "mouse", "normalized": point(event)?})),
        // Relative motion while the pointer is locked, as fractions of the picture.
        "d" => Some(json!({"action": "mouse", "delta": {
            "x": number(event, "dx")?.clamp(-4.0, 4.0),
            "y": number(event, "dy")?.clamp(-4.0, 4.0),
        }})),
        "b" => {
            // DOM button numbers.
            let button = match event.get("b")?.as_u64()? {
                0 => "Left",
                1 => "Middle",
                2 => "Right",
                3 => "Back",
                4 => "Forward",
                _ => return None,
            };
            let mut input = json!({"action": "mouse", "button": button, "pressed": event.get("down")?.as_bool()?});
            if let Some(point) = point(event) {
                input["normalized"] = point;
            }
            Some(input)
        }
        "w" => {
            // DOM wheel pixels grow downwards and to the right; one notch is
            // 100 of them and 120 Input System units, which grow upwards.
            let scroll = |key| number(event, key).unwrap_or(0.0).clamp(-2000.0, 2000.0) * -1.2;
            let mut input =
                json!({"action": "mouse", "scroll": {"x": -scroll("dx"), "y": scroll("dy")}});
            if let Some(point) = point(event) {
                input["normalized"] = point;
            }
            Some(input)
        }
        _ => None,
    }
}

/// The Player input arguments of a viewer's events, in order.
pub fn translate(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .take(MAXIMUM_EVENTS)
        .filter_map(translate_one)
        .collect()
}
