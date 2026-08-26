//! # In-process Redis test double (`fred::Mocks`)
//!
//! A functional RESP-behaviour double standing in for the external Redis
//! boundary (same category as `wiremock` for the auth URL): it implements the
//! command subset Hydra's cluster subsystems use, with REAL semantics —
//! including the Lua scripts (`EVAL`) whose behaviour is mirrored here from
//! the same script constants, so a drift between script and double would fail
//! the compile-time `assert_eq!`s at the bottom.
//!
//! Supported: `GET SET(+NX/XX+EX/PX) DEL EXISTS EXPIRE PEXPIRE INCR`,
//! `HSET HGETALL HDEL`, `SADD SREM SMEMBERS`, `ZADD ZREMRANGEBYSCORE ZCARD`,
//! `XADD XREAD XTRIM`, `EVAL` (lease renew + sliding-window scripts).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::num::ParseIntError;

use fred::error::{Error, ErrorKind};
use fred::mocks::{MockCommand, Mocks};
use fred::types::Value;

use crate::redis::rate_limit::{ADD_TOKENS_SCRIPT, CHECK_AND_INC_SCRIPT};
use crate::redis::RENEW_SCRIPT;

/// One stored value: a typed payload + an optional TTL.
#[derive(Debug)]
enum Entry {
    Bytes(Vec<u8>),
    Set(HashSet<Vec<u8>>),
    Hash(HashMap<Vec<u8>, Vec<u8>>),
    ZSet(BTreeMap<String, i64>), // member → score (ms epoch)
    Stream(VecDeque<(String, Vec<(String, String)>)>),
}

impl Entry {
    fn zset() -> Self {
        Entry::ZSet(BTreeMap::new())
    }
}

/// The in-process Redis double. Thread-safe; commands are serialised.
#[derive(Debug, Default)]
pub struct MockRedis {
    data: std::sync::Mutex<Store>,
    stream_seq: std::sync::Mutex<u64>,
}

/// `key → (payload, optional TTL)`.
type Store = HashMap<Vec<u8>, (Entry, Option<Instant>)>;

impl MockRedis {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn expired(expire_at: &Option<Instant>) -> bool {
        expire_at.map(|e| Instant::now() >= e).unwrap_or(false)
    }

    fn lookup_mut<'a>(
        map: &'a mut HashMap<Vec<u8>, (Entry, Option<Instant>)>,
        key: &[u8],
    ) -> Option<&'a mut (Entry, Option<Instant>)> {
        map.get_mut(key).filter(|(_, exp)| !Self::expired(exp))
    }

    /// Convenience for tests: run one of Hydra's Lua scripts directly against
    /// the double (fred's mock layer does not support EVAL round-trips).
    pub fn run_script(
        &self,
        script: &str,
        keys: &[String],
        argv: &[String],
    ) -> Result<i64, String> {
        match self.eval_script(script, keys, argv) {
            Ok(Value::Integer(n)) => Ok(n),
            Ok(other) => Err(format!("unexpected script result: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }

    fn process(&self, cmd: &str, args: &[Value]) -> Result<Value, Error> {
        let mut g = self.data.lock().expect("mock redis data");
        let key = |v: &Value| -> Vec<u8> { v.as_bytes().map(|b| b.to_vec()).unwrap_or_default() };
        let argstr = |v: &Value| -> String {
            match v {
                Value::String(s) => s.to_string(),
                Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                Value::Integer(i) => i.to_string(),
                Value::Boolean(b) => b.to_string(),
                _ => String::new(),
            }
        };
        match cmd {
            "GET" => {
                let k = key(&args[0]);
                match Self::lookup_mut(&mut g, &k) {
                    Some((Entry::Bytes(b), _)) => Ok(Value::Bytes(b.clone().into())),
                    _ => Ok(Value::Null),
                }
            }
            "SET" => {
                let k = key(&args[0]);
                let v = args[1].as_bytes().map(|b| b.to_vec()).unwrap_or_default();
                let mut nx = false;
                let mut xx = false;
                let mut ttl_ms: Option<i64> = None;
                let mut i = 2;
                while i < args.len() {
                    match argstr(&args[i]).as_str() {
                        "NX" => nx = true,
                        "XX" => xx = true,
                        "EX" => {
                            i += 1;
                            ttl_ms = Some(
                                argstr(&args[i])
                                    .parse::<i64>()
                                    .map_err(|e| bad(e.to_string()))?
                                    * 1000,
                            );
                        }
                        "PX" => {
                            i += 1;
                            ttl_ms = Some(
                                argstr(&args[i])
                                    .parse::<i64>()
                                    .map_err(|e| bad(e.to_string()))?,
                            );
                        }
                        "GET" => {}
                        other => return Err(bad(format!("unhandled SET option {other}"))),
                    }
                    i += 1;
                }
                let exists = Self::lookup_mut(&mut g, &k).is_some();
                if nx && exists || xx && !exists {
                    return Ok(Value::Null);
                }
                let expire_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms as u64));
                g.insert(k, (Entry::Bytes(v), expire_at));
                Ok(Value::Bytes(b"OK".to_vec().into()))
            }
            "DEL" => {
                let mut n = 0i64;
                for a in args {
                    if g.remove(&key(a)).is_some() {
                        n += 1;
                    }
                }
                Ok(Value::Integer(n))
            }
            "EXISTS" => {
                let n = args
                    .iter()
                    .filter(|a| Self::lookup_mut(&mut g, &key(a)).is_some())
                    .count() as i64;
                Ok(Value::Integer(n))
            }
            "PTTL" => {
                let k = key(&args[0]);
                match Self::lookup_mut(&mut g, &k) {
                    Some((_, Some(exp))) => {
                        let left = exp.saturating_duration_since(Instant::now()).as_millis() as i64;
                        Ok(Value::Integer(left))
                    }
                    _ => Ok(Value::Integer(-2)), // missing key
                }
            }
            "EXPIRE" | "PEXPIRE" => {
                let k = key(&args[0]);
                let n: i64 = argstr(&args[1])
                    .parse::<i64>()
                    .map_err(|e| bad(e.to_string()))?;
                let ms = if cmd == "EXPIRE" { n * 1000 } else { n };
                match Self::lookup_mut(&mut g, &k) {
                    Some((_, exp)) => {
                        *exp = Some(Instant::now() + Duration::from_millis(ms as u64));
                        Ok(Value::Integer(1))
                    }
                    None => Ok(Value::Integer(0)),
                }
            }
            "INCR" => {
                let k = key(&args[0]);
                let cur: i64 = match Self::lookup_mut(&mut g, &k) {
                    Some((Entry::Bytes(b), _)) => String::from_utf8_lossy(b)
                        .parse::<i64>()
                        .map_err(|e| bad(e.to_string()))?,
                    _ => 0,
                };
                let next = cur + 1;
                g.insert(k, (Entry::Bytes(next.to_string().into_bytes()), None));
                Ok(Value::Integer(next))
            }
            "HSET" => {
                let k = key(&args[0]);
                let e = g
                    .entry(k)
                    .or_insert_with(|| (Entry::Hash(HashMap::new()), None));
                let Entry::Hash(map) = &mut e.0 else {
                    return Err(bad("wrongtype HSET"));
                };
                let mut n = 0i64;
                let mut i = 1;
                while i + 1 < args.len() {
                    let f = key(&args[i]);
                    let v = key(&args[i + 1]);
                    if map.insert(f, v).is_none() {
                        n += 1;
                    }
                    i += 2;
                }
                Ok(Value::Integer(n))
            }
            "HGETALL" => {
                let k = key(&args[0]);
                match Self::lookup_mut(&mut g, &k) {
                    Some((Entry::Hash(map), _)) => {
                        let mut out = Vec::with_capacity(map.len() * 2);
                        for (f, v) in map {
                            out.push(Value::Bytes(f.clone().into()));
                            out.push(Value::Bytes(v.clone().into()));
                        }
                        Ok(Value::Array(out))
                    }
                    _ => Ok(Value::Array(vec![])),
                }
            }
            "HDEL" => {
                let k = key(&args[0]);
                let mut n = 0i64;
                if let Some((Entry::Hash(map), _)) = Self::lookup_mut(&mut g, &k) {
                    for a in &args[1..] {
                        if map.remove(&key(a)).is_some() {
                            n += 1;
                        }
                    }
                }
                Ok(Value::Integer(n))
            }
            "HGET" => {
                let k = key(&args[0]);
                match Self::lookup_mut(&mut g, &k) {
                    Some((Entry::Hash(map), _)) => match map.get(&key(&args[1])) {
                        Some(v) => Ok(Value::Bytes(v.clone().into())),
                        None => Ok(Value::Null),
                    },
                    _ => Ok(Value::Null),
                }
            }
            "SADD" => {
                let k = key(&args[0]);
                let e = g
                    .entry(k)
                    .or_insert_with(|| (Entry::Set(HashSet::new()), None));
                let Entry::Set(set) = &mut e.0 else {
                    return Err(bad("wrongtype SADD"));
                };
                let mut n = 0i64;
                for a in &args[1..] {
                    if set.insert(key(a)) {
                        n += 1;
                    }
                }
                Ok(Value::Integer(n))
            }
            "SREM" => {
                let k = key(&args[0]);
                let mut n = 0i64;
                if let Some((Entry::Set(set), _)) = Self::lookup_mut(&mut g, &k) {
                    for a in &args[1..] {
                        if set.remove(&key(a)) {
                            n += 1;
                        }
                    }
                }
                Ok(Value::Integer(n))
            }
            "SMEMBERS" => {
                let k = key(&args[0]);
                match Self::lookup_mut(&mut g, &k) {
                    Some((Entry::Set(set), _)) => Ok(Value::Array(
                        set.iter().map(|m| Value::Bytes(m.clone().into())).collect(),
                    )),
                    _ => Ok(Value::Array(vec![])),
                }
            }
            "ZADD" => {
                let k = key(&args[0]);
                let e = g.entry(k).or_insert_with(|| (Entry::zset(), None));
                let Entry::ZSet(z) = &mut e.0 else {
                    return Err(bad("wrongtype ZADD"));
                };
                let mut i = 1;
                while i + 1 < args.len() {
                    let score: i64 = argstr(&args[i])
                        .parse::<i64>()
                        .map_err(|e| bad(e.to_string()))?;
                    let member = argstr(&args[i + 1]);
                    z.insert(member, score);
                    i += 2;
                }
                Ok(Value::Integer(z.len() as i64))
            }
            "ZREMRANGEBYSCORE" => {
                let k = key(&args[0]);
                if let Some((Entry::ZSet(z), _)) = Self::lookup_mut(&mut g, &k) {
                    let min: f64 = argstr(&args[1])
                        .parse::<f64>()
                        .map_err(|e| bad(e.to_string()))?;
                    let max: f64 = argstr(&args[2])
                        .parse::<f64>()
                        .map_err(|e| bad(e.to_string()))?;
                    let before = z.len();
                    z.retain(|_, s| (*s as f64) < min || (*s as f64) > max);
                    return Ok(Value::Integer((before - z.len()) as i64));
                }
                Ok(Value::Integer(0))
            }
            "ZCARD" => {
                let k = key(&args[0]);
                match Self::lookup_mut(&mut g, &k) {
                    Some((Entry::ZSet(z), _)) => Ok(Value::Integer(z.len() as i64)),
                    _ => Ok(Value::Integer(0)),
                }
            }
            "XADD" => {
                let k = key(&args[0]);
                // XADD key * field value [field value...]
                let seq = {
                    let mut s = self.stream_seq.lock().expect("mock stream seq");
                    *s += 1;
                    *s
                };
                let id = format!("{}-{}", Self::now_ms(), seq);
                let mut fields = Vec::new();
                let mut i = 2;
                while i + 1 < args.len() {
                    fields.push((argstr(&args[i]), argstr(&args[i + 1])));
                    i += 2;
                }
                let e = g
                    .entry(k)
                    .or_insert_with(|| (Entry::Stream(VecDeque::new()), None));
                let Entry::Stream(q) = &mut e.0 else {
                    return Err(bad("wrongtype XADD"));
                };
                q.push_back((id.clone(), fields));
                Ok(Value::Bytes(id.into_bytes().into()))
            }
            "XREAD" => {
                // XREAD [COUNT n] STREAMS key... id...
                let mut count: usize = usize::MAX;
                let mut i = 0;
                if args.first().is_some_and(|a| argstr(a) == "COUNT") {
                    count = argstr(&args[1])
                        .parse()
                        .map_err(|e: ParseIntError| bad(e.to_string()))?;
                    i = 2;
                }
                // args[i] == "STREAMS"
                let nkeys = (args.len() - i - 1) / 2;
                let mut out = Vec::new();
                for ki in 0..nkeys {
                    let k = key(&args[i + 1 + ki]);
                    let since = argstr(&args[i + 1 + nkeys + ki]);
                    let (id_prefix, _) = since.split_once('-').unwrap_or((&since, ""));
                    let since_num: i64 = id_prefix.parse().unwrap_or(0);
                    if let Some((Entry::Stream(q), _)) = Self::lookup_mut(&mut g, &k) {
                        let mut rows = Vec::new();
                        for (id, fields) in q.iter() {
                            if rows.len() >= count {
                                break;
                            }
                            let (idn, _) = id.split_once('-').unwrap_or((id.as_str(), ""));
                            let idn: i64 = idn.parse().unwrap_or(0);
                            if idn > since_num || (idn == since_num && id.as_str() > since.as_str())
                            {
                                let mut fv = Vec::new();
                                for (f, v) in fields {
                                    fv.push(Value::Bytes(f.as_bytes().to_vec().into()));
                                    fv.push(Value::Bytes(v.as_bytes().to_vec().into()));
                                }
                                rows.push(Value::Array(vec![
                                    Value::Bytes(id.as_bytes().to_vec().into()),
                                    Value::Array(fv),
                                ]));
                            }
                        }
                        if !rows.is_empty() {
                            out.push(Value::Array(vec![
                                Value::Bytes(k.clone().into()),
                                Value::Array(rows),
                            ]));
                        }
                    }
                }
                Ok(Value::Array(out))
            }
            "XTRIM" => {
                let k = key(&args[0]);
                // XTRIM key MAXLEN [~] n
                let maxlen: usize = args
                    .iter()
                    .rfind(|a| argstr(a).parse::<usize>().is_ok())
                    .and_then(|a| argstr(a).parse().ok())
                    .unwrap_or(0);
                if let Some((Entry::Stream(q), _)) = Self::lookup_mut(&mut g, &k) {
                    let removed = q.len().saturating_sub(maxlen) as i64;
                    while q.len() > maxlen {
                        q.pop_front();
                    }
                    return Ok(Value::Integer(removed));
                }
                Ok(Value::Integer(0))
            }
            "EVAL" => {
                let script = argstr(&args[0]);
                let numkeys: usize = argstr(&args[1])
                    .parse()
                    .map_err(|e: ParseIntError| bad(e.to_string()))?;
                let keys: Vec<String> = args[2..2 + numkeys].iter().map(argstr).collect();
                let argv: Vec<String> = args[2 + numkeys..].iter().map(argstr).collect();
                self.eval_script(&script, &keys, &argv)
            }
            other => Err(bad(format!("mock redis: unhandled command {other}"))),
        }
    }

    /// Execute the Lua scripts Hydra ships. Each branch mirrors the script's
    /// semantics exactly (the script CONSTANTS are shared, so a drift breaks
    /// the `assert_eq!` checks below rather than silently diverging).
    pub(crate) fn eval_script(
        &self,
        script: &str,
        keys: &[String],
        argv: &[String],
    ) -> Result<Value, Error> {
        let mut g = self.data.lock().expect("mock redis data");
        if script == RENEW_SCRIPT {
            let holder = match Self::lookup_mut(&mut g, keys[0].as_bytes()) {
                Some((Entry::Bytes(b), _)) => String::from_utf8_lossy(b).into_owned(),
                _ => String::new(),
            };
            if holder == argv[0] {
                let ms: u64 = argv[1].parse::<u64>().map_err(|e| bad(e.to_string()))?;
                if let Some((_, exp)) = Self::lookup_mut(&mut g, keys[0].as_bytes()) {
                    *exp = Some(Instant::now() + Duration::from_millis(ms));
                }
                return Ok(Value::Integer(1));
            }
            return Ok(Value::Integer(0));
        }
        if script == CHECK_AND_INC_SCRIPT {
            // argv: [now_ms, window_ms, limit, member]
            let now: i64 = argv[0]
                .parse()
                .map_err(|e: ParseIntError| bad(e.to_string()))?;
            let window: i64 = argv[1]
                .parse()
                .map_err(|e: ParseIntError| bad(e.to_string()))?;
            let limit: i64 = argv[2]
                .parse()
                .map_err(|e: ParseIntError| bad(e.to_string()))?;
            let member = argv[3].clone();
            let k = keys[0].as_bytes();
            let e = g.entry(k.to_vec()).or_insert_with(|| (Entry::zset(), None));
            let Entry::ZSet(z) = &mut e.0 else {
                return Err(bad("wrongtype EVAL check (count key)"));
            };
            z.retain(|_, s| *s >= now - window);
            if (z.len() as i64) < limit {
                z.insert(member, now);
                e.1 = Some(Instant::now() + Duration::from_millis(window as u64));
                return Ok(Value::Integer(1));
            }
            return Ok(Value::Integer(0));
        }
        if script == ADD_TOKENS_SCRIPT {
            // argv: [now_ms, window_ms, member]
            let now: i64 = argv[0]
                .parse()
                .map_err(|e: ParseIntError| bad(e.to_string()))?;
            let window: i64 = argv[1]
                .parse()
                .map_err(|e: ParseIntError| bad(e.to_string()))?;
            let member = argv[2].clone();
            let k = keys[0].as_bytes();
            let e = g.entry(k.to_vec()).or_insert_with(|| (Entry::zset(), None));
            let Entry::ZSet(z) = &mut e.0 else {
                return Err(bad("wrongtype EVAL add (tokens key)"));
            };
            z.retain(|_, s| *s >= now - window);
            z.insert(member, now);
            e.1 = Some(Instant::now() + Duration::from_millis(window as u64));
            return Ok(Value::Integer(z.len() as i64));
        }
        Err(bad(format!(
            "mock redis: unhandled EVAL script {:?}",
            &script[..script.len().min(40)]
        )))
    }
}

impl Mocks for MockRedis {
    fn process_command(&self, command: MockCommand) -> Result<Value, Error> {
        self.process(&command.cmd, &command.args)
    }
}

fn bad(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Unknown, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis::rate_limit::{ADD_TOKENS_SCRIPT, CHECK_AND_INC_SCRIPT};
    use fred::prelude::*;
    use fred::types::{Expiration, SetOptions};

    fn pool() -> Pool {
        let mock = std::sync::Arc::new(MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        Pool::new(cfg, None, None, None, 1).expect("pool")
    }

    #[tokio::test]
    async fn set_nx_px_semantics() {
        let client = pool();
        client.init().await.unwrap();
        let ok: Option<String> = client
            .set(
                "k",
                "a",
                Some(Expiration::PX(50_000)),
                Some(SetOptions::NX),
                false,
            )
            .await
            .unwrap();
        assert_eq!(ok.as_deref(), Some("OK"));
        let nil: Option<String> = client
            .set(
                "k",
                "b",
                Some(Expiration::PX(50_000)),
                Some(SetOptions::NX),
                false,
            )
            .await
            .unwrap();
        assert!(nil.is_none(), "NX must not overwrite");
        let ok: Option<String> = client
            .set(
                "k",
                "c",
                Some(Expiration::PX(50_000)),
                Some(SetOptions::XX),
                false,
            )
            .await
            .unwrap();
        assert_eq!(ok.as_deref(), Some("OK"));
        let v: String = client.get("k").await.unwrap();
        assert_eq!(v, "c");
    }

    #[test]
    fn scripts_semantics_direct() {
        let m = MockRedis::new();
        let lease_key = "hydra:{lease:leader}".to_string();
        // RENEW: not held → 0
        assert_eq!(
            m.run_script(
                RENEW_SCRIPT,
                &[lease_key.clone()],
                &["n1".into(), "5000".into()]
            )
            .unwrap(),
            0
        );
        // Acquire (direct SET), then renew as holder → 1; as another node → 0.
        m.process(
            "SET",
            &[
                Value::Bytes(lease_key.clone().into()),
                Value::Bytes("n1".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            m.run_script(
                RENEW_SCRIPT,
                &[lease_key.clone()],
                &["n1".into(), "5000".into()]
            )
            .unwrap(),
            1
        );
        assert_eq!(
            m.run_script(
                RENEW_SCRIPT,
                &[lease_key.clone()],
                &["n2".into(), "5000".into()]
            )
            .unwrap(),
            0
        );

        // Sliding window (count): limit 2 → admit, admit, deny.
        let ck = "hydra:{rl:r1:b}:count".to_string();
        for (i, expect) in [1i64, 1, 0].iter().enumerate() {
            let got = m
                .run_script(
                    CHECK_AND_INC_SCRIPT,
                    &[ck.clone()],
                    &["1000".into(), "60000".into(), "2".into(), format!("m{i}")],
                )
                .unwrap();
            assert_eq!(got, *expect, "call {i}");
        }
        // Token accounting adds a sample.
        let tk = "hydra:{rl:r1:b}:tokens".to_string();
        let n = m
            .run_script(
                ADD_TOKENS_SCRIPT,
                &[tk],
                &["1000".into(), "60000".into(), "m9".into()],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
}
