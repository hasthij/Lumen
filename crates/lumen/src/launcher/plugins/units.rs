//! Unit converter: "10 km to mi", "72 f in c", "1.5 gb mb", "5 kg".

use super::calc::format_number;
use crate::launcher::item::{Action, Item, Kind};

#[derive(Clone, Copy, PartialEq)]
enum Cat {
    Length,
    Mass,
    Temp,
    Volume,
    Data,
    Speed,
    Time,
    Area,
}

/// (names, category, factor to the category's base unit). Temperature is special-cased.
const UNITS: &[(&[&str], Cat, f64)] = &[
    (&["mm", "millimeter", "millimeters"], Cat::Length, 0.001),
    (&["cm", "centimeter", "centimeters"], Cat::Length, 0.01),
    (&["m", "meter", "meters", "metre", "metres"], Cat::Length, 1.0),
    (&["km", "kilometer", "kilometers"], Cat::Length, 1000.0),
    (&["in", "inch", "inches", "\""], Cat::Length, 0.0254),
    (&["ft", "foot", "feet", "'"], Cat::Length, 0.3048),
    (&["yd", "yard", "yards"], Cat::Length, 0.9144),
    (&["mi", "mile", "miles"], Cat::Length, 1609.344),
    (&["nmi", "nauticalmile"], Cat::Length, 1852.0),
    (&["mg", "milligram", "milligrams"], Cat::Mass, 0.001),
    (&["g", "gram", "grams"], Cat::Mass, 1.0),
    (&["kg", "kilogram", "kilograms", "kilo", "kilos"], Cat::Mass, 1000.0),
    (&["t", "tonne", "tonnes"], Cat::Mass, 1_000_000.0),
    (&["oz", "ounce", "ounces"], Cat::Mass, 28.349523125),
    (&["lb", "lbs", "pound", "pounds"], Cat::Mass, 453.59237),
    (&["st", "stone"], Cat::Mass, 6350.29318),
    (&["c", "°c", "celsius"], Cat::Temp, 0.0),
    (&["f", "°f", "fahrenheit"], Cat::Temp, 1.0),
    (&["k", "kelvin"], Cat::Temp, 2.0),
    (&["ml", "milliliter", "milliliters"], Cat::Volume, 0.001),
    (&["l", "liter", "liters", "litre", "litres"], Cat::Volume, 1.0),
    (&["m3", "cubicmeter"], Cat::Volume, 1000.0),
    (&["gal", "gallon", "gallons"], Cat::Volume, 3.785411784),
    (&["qt", "quart", "quarts"], Cat::Volume, 0.946352946),
    (&["pt", "pint", "pints"], Cat::Volume, 0.473176473),
    (&["cup", "cups"], Cat::Volume, 0.2365882365),
    (&["floz", "fluidounce"], Cat::Volume, 0.0295735295625),
    (&["bit", "bits"], Cat::Data, 0.125),
    (&["b", "byte", "bytes"], Cat::Data, 1.0),
    (&["kb", "kilobyte", "kilobytes"], Cat::Data, 1e3),
    (&["mb", "megabyte", "megabytes"], Cat::Data, 1e6),
    (&["gb", "gigabyte", "gigabytes"], Cat::Data, 1e9),
    (&["tb", "terabyte", "terabytes"], Cat::Data, 1e12),
    (&["kib"], Cat::Data, 1024.0),
    (&["mib"], Cat::Data, 1048576.0),
    (&["gib"], Cat::Data, 1073741824.0),
    (&["tib"], Cat::Data, 1099511627776.0),
    (&["m/s", "mps"], Cat::Speed, 1.0),
    (&["km/h", "kmh", "kph"], Cat::Speed, 1.0 / 3.6),
    (&["mph"], Cat::Speed, 0.44704),
    (&["kn", "knot", "knots"], Cat::Speed, 0.514444),
    (&["ms", "millisecond", "milliseconds"], Cat::Time, 0.001),
    (&["s", "sec", "second", "seconds"], Cat::Time, 1.0),
    (&["min", "minute", "minutes"], Cat::Time, 60.0),
    (&["h", "hr", "hour", "hours"], Cat::Time, 3600.0),
    (&["d", "day", "days"], Cat::Time, 86400.0),
    (&["wk", "week", "weeks"], Cat::Time, 604800.0),
    (&["yr", "year", "years"], Cat::Time, 31557600.0),
    (&["m2", "sqm"], Cat::Area, 1.0),
    (&["km2", "sqkm"], Cat::Area, 1e6),
    (&["ft2", "sqft"], Cat::Area, 0.09290304),
    (&["ac", "acre", "acres"], Cat::Area, 4046.8564224),
    (&["ha", "hectare", "hectares"], Cat::Area, 10000.0),
];

fn unit(name: &str) -> Option<usize> {
    let n = name.trim().to_lowercase();
    UNITS.iter().position(|(names, _, _)| names.contains(&n.as_str()))
}

fn to_base(v: f64, u: usize) -> f64 {
    let (_, cat, f) = UNITS[u];
    match (cat, f as u8) {
        (Cat::Temp, 0) => v,
        (Cat::Temp, 1) => (v - 32.0) * 5.0 / 9.0,
        (Cat::Temp, _) => v - 273.15,
        _ => v * f,
    }
}

fn from_base(v: f64, u: usize) -> f64 {
    let (_, cat, f) = UNITS[u];
    match (cat, f as u8) {
        (Cat::Temp, 0) => v,
        (Cat::Temp, 1) => v * 9.0 / 5.0 + 32.0,
        (Cat::Temp, _) => v + 273.15,
        _ => v / f,
    }
}

pub fn query(q: &str, out: &mut Vec<Item>) {
    let q = q.trim().to_lowercase();
    // Split the leading number from the rest: "10km to mi" and "10 km to mi" both work.
    let num_end = q.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == ',')).unwrap_or(q.len());
    let Ok(value) = q[..num_end].replace(',', "").parse::<f64>() else { return };
    let rest: Vec<&str> = q[num_end..].split_whitespace().filter(|w| !matches!(*w, "to" | "in" | "as" | ">" | "=")).collect();
    let Some(from) = rest.first().and_then(|u| unit(u)) else { return };
    let cat = UNITS[from].1;
    let targets: Vec<usize> = match rest.get(1).and_then(|u| unit(u)) {
        Some(t) if UNITS[t].1 == cat => vec![t],
        Some(_) => return,
        None => (0..UNITS.len()).filter(|&i| i != from && UNITS[i].1 == cat).collect(),
    };
    let base = to_base(value, from);
    for (rank, t) in targets.into_iter().enumerate() {
        let result = format_number((from_base(base, t) * 1e9).round() / 1e9);
        let unit_name = UNITS[t].0[0];
        out.push(
            Item::new(Kind::Units, format!("{result} {unit_name}"), format!("{} {}  ·  Enter to copy", format_number(value), UNITS[from].0[0]), Action::Copy(result.clone()))
                .key(format!("units:{unit_name}"))
                .score(4000 - rank as i32),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert() {
        let mut out = Vec::new();
        query("10 km to mi", &mut out);
        assert_eq!(out[0].title, "6.213711922 mi");
        out.clear();
        query("100 c f", &mut out);
        assert_eq!(out[0].title, "212 f");
        out.clear();
        query("1 gib mb", &mut out);
        assert_eq!(out[0].title, "1073.741824 mb");
    }
}
