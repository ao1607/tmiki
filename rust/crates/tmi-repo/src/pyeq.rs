//! Python's `==` between JSON-shaped values (`1 == 1.0 == True`, dicts by
//! their items, lists by their elements), as tmi compares stored layer
//! records with freshly imported ones.

use tmi_store::json::{Int, Value};

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(*b as i64 as f64),
        Value::Int(Int::Small(i)) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Int(Int::Small(i)) => Some(*i),
        _ => None,
    }
}

/// `a == b` in Python.
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        (Value::Int(Int::Big(x)), Value::Int(Int::Big(y))) => x == y,
        _ => match (int(a), int(b)) {
            (Some(x), Some(y)) => x == y,
            _ => match (num(a), num(b)) {
                // int vs float: exact for the ints JSON gives tmi (|i| < 2**53)
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        },
    }
}

pub fn py_ne(a: &Value, b: &Value) -> bool {
    !py_eq(a, b)
}

/// `dict.get(key)` of an object value (None: absent or not an object).
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object()?.get(key)
}

/// `d.get(key)` as a value (`None` when absent).
pub fn get_or_null(v: &Value, key: &str) -> Value {
    get(v, key).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmi_store::json::loads;

    #[test]
    fn python_equality() {
        assert!(py_eq(&loads("1").unwrap(), &loads("1.0").unwrap()));
        assert!(py_eq(&loads("true").unwrap(), &loads("1").unwrap()));
        assert!(!py_eq(&loads("\"1\"").unwrap(), &loads("1").unwrap()));
        assert!(py_eq(&loads(r#"{"a":[1,2],"b":null}"#).unwrap(), &loads(r#"{"b":null,"a":[1.0,2]}"#).unwrap()));
        assert!(!py_eq(&loads("NaN").unwrap(), &loads("NaN").unwrap()));
        assert!(!py_eq(&loads("null").unwrap(), &loads("0").unwrap()));
    }
}
