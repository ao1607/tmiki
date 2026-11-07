//! The four messages (the crate's doc).

use crate::cbor::{Value, ValueError};

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// tmi calls an export: `{"call": name, "args": [..]}`.
    Call { func: String, args: Vec<Value> },
    /// The plugin calls a host function: `{"import": name, "args": [..]}`.
    Import { func: String, args: Vec<Value> },
    /// `{"return": value}`.
    Return(Value),
    /// `{"trap": message}`.
    Trap(String),
}

impl Message {
    pub fn to_value(&self) -> Value {
        match self {
            Message::Call { func, args } => Value::record([("call", Value::text(func.clone())), ("args", Value::Array(args.clone()))]),
            Message::Import { func, args } => Value::record([("import", Value::text(func.clone())), ("args", Value::Array(args.clone()))]),
            Message::Return(v) => Value::record([("return", v.clone())]),
            Message::Trap(m) => Value::record([("trap", Value::text(m.clone()))]),
        }
    }

    /// `to_value` without copying the payload.
    pub fn into_value(self) -> Value {
        match self {
            Message::Call { func, args } => Value::record([("call", Value::Text(func)), ("args", Value::Array(args))]),
            Message::Import { func, args } => Value::record([("import", Value::Text(func)), ("args", Value::Array(args))]),
            Message::Return(v) => Value::record([("return", v)]),
            Message::Trap(m) => Value::record([("trap", Value::Text(m))]),
        }
    }

    pub fn from_value(v: Value) -> Result<Message, ValueError> {
        let Value::Map(entries) = v else {
            return Err(ValueError(format!("a message is a map, not {}", v.kind())));
        };
        let mut name: Option<(String, String)> = None;
        let mut args: Option<Vec<Value>> = None;
        let mut ret: Option<Value> = None;
        let mut trap: Option<String> = None;
        for (k, v) in entries {
            let Value::Text(k) = k else { return Err(ValueError("a message's keys are text strings".into())) };
            match k.as_str() {
                "call" | "import" => name = Some((k, v.string("the message's name")?)),
                "args" => match v {
                    Value::Array(a) => args = Some(a),
                    other => return Err(ValueError(format!("args: expected an array, got {}", other.kind()))),
                },
                "return" => ret = Some(v),
                "trap" => trap = Some(v.string("trap")?),
                other => return Err(ValueError(format!("a message has no key {other:?}"))),
            }
        }
        match (name, args, ret, trap) {
            (Some((k, func)), Some(args), None, None) if k == "call" => Ok(Message::Call { func, args }),
            (Some((_, func)), Some(args), None, None) => Ok(Message::Import { func, args }),
            (None, None, Some(v), None) => Ok(Message::Return(v)),
            (None, None, None, Some(m)) => Ok(Message::Trap(m)),
            _ => Err(ValueError("a message is one of {call, args}, {import, args}, {return} or {trap}".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_go_round() {
        for m in [
            Message::Call { func: "reader.info".into(), args: vec![] },
            Message::Import { func: "source.source-file.read".into(), args: vec![Value::Uint(0), Value::Uint(4), Value::Uint(8)] },
            Message::Return(Value::ok(Value::Null)),
            Message::Trap("boom".into()),
        ] {
            assert_eq!(Message::from_value(Value::decode(&m.to_value().encode()).unwrap()).unwrap(), m);
        }
        assert!(Message::from_value(Value::record([("return", Value::Null), ("trap", Value::text("x"))])).is_err());
        assert!(Message::from_value(Value::Uint(1)).is_err());
    }
}
