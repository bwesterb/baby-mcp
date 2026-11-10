//! The field registry: the only place structured fields are defined. Static
//! by design; adding a field means adding it here (its generated SQL column
//! is created automatically at startup).

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Number { min: f64, max: f64 },
    Integer { min: i64, max: i64 },
    Bool,
    Choice(&'static [&'static str]),
}

/// Groups decide what counts as a likely duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Feed,
    Diaper,
    Vomit,
    Measure,
    Position,
    Care,
    Supplement,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::Feed => "feed",
            Group::Diaper => "diaper",
            Group::Vomit => "vomit",
            Group::Measure => "measurement",
            Group::Position => "position",
            Group::Care => "care",
            Group::Supplement => "supplement",
        }
    }
}

pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    pub unit: &'static str,
    pub group: Group,
    pub help: &'static str,
}

pub const LATCH: &[&str] = &["with_aid", "with_difficulty", "without_difficulty", "none"];
pub const SIDES: &[&str] = &["left", "right"];
pub const BATH: &[&str] = &["with_soap", "without_soap", "sponge"];
pub const STOOL: &[&str] = &["meconium", "green", "brown", "yellow", "transitional", "other"];

pub static FIELDS: &[Field] = &[
    Field {
        name: "sleep_facing",
        kind: Kind::Choice(SIDES),
        unit: "",
        group: Group::Position,
        help: "the side the face is turned to when put down to sleep (not the breast side); the parents alternate it after every feed",
    },
    Field {
        name: "bottle_breastmilk_ml",
        kind: Kind::Number { min: 0.0, max: 300.0 },
        unit: "ml",
        group: Group::Feed,
        help: "expressed (pumped) breast milk given by bottle, cup, finger or syringe",
    },
    Field {
        name: "bottle_formula_ml",
        kind: Kind::Number { min: 0.0, max: 300.0 },
        unit: "ml",
        group: Group::Feed,
        help: "formula given by bottle; a mixed bottle fills both bottle fields",
    },
    Field {
        name: "breast_left_min",
        kind: Kind::Integer { min: 0, max: 90 },
        unit: "min",
        group: Group::Feed,
        help: "minutes at the left breast",
    },
    Field {
        name: "breast_right_min",
        kind: Kind::Integer { min: 0, max: 90 },
        unit: "min",
        group: Group::Feed,
        help: "minutes at the right breast",
    },
    Field {
        name: "latch",
        kind: Kind::Choice(LATCH),
        unit: "",
        group: Group::Feed,
        help: "put to the breast: with_aid (e.g. nipple shield), with_difficulty, without_difficulty, or none (not put to the breast)",
    },
    Field {
        name: "wet",
        kind: Kind::Bool,
        unit: "",
        group: Group::Diaper,
        help: "wet diaper",
    },
    Field {
        name: "dirty",
        kind: Kind::Bool,
        unit: "",
        group: Group::Diaper,
        help: "dirty diaper (stool)",
    },
    Field {
        name: "stool_color",
        kind: Kind::Choice(STOOL),
        unit: "",
        group: Group::Diaper,
        help: "colour of the stool",
    },
    Field {
        name: "vomit",
        kind: Kind::Bool,
        unit: "",
        group: Group::Vomit,
        help: "spit up or vomited",
    },
    Field {
        name: "temp_c",
        kind: Kind::Number { min: 34.0, max: 42.0 },
        unit: "°C",
        group: Group::Measure,
        help: "body temperature",
    },
    Field {
        name: "weight_g",
        kind: Kind::Number { min: 1000.0, max: 15000.0 },
        unit: "g",
        group: Group::Measure,
        help: "weight",
    },
    Field {
        name: "bath",
        kind: Kind::Choice(BATH),
        unit: "",
        group: Group::Care,
        help: "in the bath with soap, in the bath without soap (just water), or only a sponge/washcloth wash",
    },
    Field {
        name: "vitamin_k",
        kind: Kind::Bool,
        unit: "",
        group: Group::Supplement,
        help: "vitamin K drops given",
    },
    Field {
        name: "vitamin_d",
        kind: Kind::Bool,
        unit: "",
        group: Group::Supplement,
        help: "vitamin D drops given",
    },
];

pub fn get(name: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.name == name)
}

/// The groups the given fields belong to, each once, in group order.
pub fn groups(fields: &serde_json::Map<String, Value>) -> Vec<Group> {
    let mut g: Vec<Group> = fields.keys().filter_map(|k| get(k).map(|f| f.group)).collect();
    g.sort_by_key(|g| *g as u8);
    g.dedup();
    g
}

impl Field {
    pub fn sql_type(&self) -> &'static str {
        match self.kind {
            Kind::Number { .. } => "REAL",
            Kind::Integer { .. } | Kind::Bool => "INTEGER",
            Kind::Choice(_) => "TEXT",
        }
    }

    /// Validates and normalizes a value; `null` (deliberately nothing) is
    /// always allowed.
    pub fn validate(&self, v: &Value) -> Result<Value, String> {
        if v.is_null() {
            return Ok(Value::Null);
        }
        let bad = |what: String| Err(format!("{}: {what}", self.name));
        match self.kind {
            Kind::Number { min, max } => match v.as_f64() {
                Some(x) if (min..=max).contains(&x) => Ok(json!(x)),
                Some(x) => bad(format!("{x} is outside {min}–{max} {}", self.unit)),
                None => bad(format!("expected a number in {}, got {v}", self.unit)),
            },
            Kind::Integer { min, max } => match v.as_f64() {
                Some(x) if x.fract() == 0.0 && (min as f64..=max as f64).contains(&x) => {
                    Ok(json!(x as i64))
                }
                Some(x) => bad(format!("{x} is not a whole number in {min}–{max} {}", self.unit)),
                None => bad(format!("expected a whole number, got {v}")),
            },
            Kind::Bool => match v.as_bool() {
                Some(b) => Ok(json!(b)),
                None => bad(format!("expected true or false, got {v}")),
            },
            Kind::Choice(opts) => match v.as_str() {
                Some(s) if opts.contains(&s) => Ok(json!(s)),
                _ => bad(format!("expected one of {}, got {v}", opts.join(", "))),
            },
        }
    }

    pub fn describe(&self) -> Value {
        let (typ, extra) = match self.kind {
            Kind::Number { min, max } => ("number", json!({"min": min, "max": max})),
            Kind::Integer { min, max } => ("integer", json!({"min": min, "max": max})),
            Kind::Bool => ("boolean", json!({})),
            Kind::Choice(o) => ("choice", json!({"choices": o})),
        };
        let mut d = json!({
            "name": self.name, "type": typ, "group": self.group.name(), "description": self.help,
        });
        if !self.unit.is_empty() {
            d["unit"] = json!(self.unit);
        }
        if let (Some(d), Some(e)) = (d.as_object_mut(), extra.as_object()) {
            d.extend(e.clone());
        }
        d
    }
}

/// Validates an object of fields (merging is the caller's business).
pub fn validate_fields(obj: &serde_json::Map<String, Value>) -> Result<serde_json::Map<String, Value>, String> {
    let mut out = serde_json::Map::new();
    for (k, v) in obj {
        let f = get(k).ok_or_else(|| {
            format!(
                "unknown field {k:?}; known fields: {}. Keep other details in the text (and consider `suggest`).",
                FIELDS.iter().map(|f| f.name).collect::<Vec<_>>().join(", ")
            )
        })?;
        out.insert(k.clone(), f.validate(v)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        let f = get("temp_c").unwrap();
        assert_eq!(f.validate(&json!(36.8)).unwrap(), json!(36.8));
        assert!(f.validate(&json!(368)).is_err());
        assert_eq!(f.validate(&Value::Null).unwrap(), Value::Null);
        assert_eq!(get("breast_left_min").unwrap().validate(&json!(5.0)).unwrap(), json!(5));
        assert!(get("breast_left_min").unwrap().validate(&json!(5.5)).is_err());
        assert!(get("stool_color").unwrap().validate(&json!("purple")).is_err());
        assert_eq!(get("bath").unwrap().validate(&json!("sponge")).unwrap(), json!("sponge"));
        assert!(get("bath").unwrap().validate(&json!("shower")).is_err());
        assert!(get("wet").unwrap().validate(&json!("yes")).is_err());
        let mut m = serde_json::Map::new();
        m.insert("nope".into(), json!(1));
        assert!(validate_fields(&m).is_err());
    }

    #[test]
    fn names_are_sql_identifiers() {
        for f in FIELDS {
            assert!(f.name.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()));
        }
    }
}
