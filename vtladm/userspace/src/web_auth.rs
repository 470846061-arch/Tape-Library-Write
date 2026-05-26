//! Web 登录：算术验证码、会话 cookie、bcrypt 密码文件（默认 admin，可修改）。

use bcrypt::{hash, verify, DEFAULT_COST};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const DEFAULT_WEB_USER: &str = "admin";
pub const DEFAULT_WEB_PASSWORD: &str = "4rfVBNji9";
/// Cookie `Max-Age` 与会话有效期（秒）
pub const SESSION_SECS: u64 = 86400;

const CAPTCHA_SECS: u64 = 300;
const LOGIN_WINDOW_SECS: u64 = 60;
const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_LOCK_SECS: u64 = 300;
const MAX_SESSIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFail {
    BadCredentials,
    AuthFile,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WebAuthFile {
    pub username: String,
    pub password_hash: String,
    #[serde(default)]
    pub allow_iscsi_exec: bool,
}

pub struct WebState {
    pub auth_file: PathBuf,
    captcha: Mutex<HashMap<String, (i32, Instant)>>,
    sessions: Mutex<HashMap<String, (String, Instant)>>,
    login_failures: Mutex<HashMap<String, LoginThrottle>>,
}

#[derive(Clone, Copy)]
struct LoginThrottle {
    failures: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

impl WebState {
    pub fn new(auth_file: PathBuf) -> Self {
        Self {
            auth_file,
            captcha: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            login_failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn init_auth_file(&self) -> Result<(), std::io::Error> {
        if self.auth_file.exists() {
            return Ok(());
        }
        self.write_default_auth(DEFAULT_WEB_PASSWORD)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    /// 强制重写 `web_admin.json`（恢复默认用户 `admin` 与新密码哈希）。
    pub fn force_reset_auth(&self, password: &str) -> Result<(), String> {
        if password.len() < 8 {
            return Err("密码至少 8 个字符".into());
        }
        self.write_default_auth(password)
    }

    fn write_default_auth(&self, password: &str) -> Result<(), String> {
        if let Some(p) = self.auth_file.parent() {
            fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let password_hash = hash(password, DEFAULT_COST).map_err(|e| e.to_string())?;
        let cred = WebAuthFile {
            username: DEFAULT_WEB_USER.to_string(),
            password_hash,
            allow_iscsi_exec: false,
        };
        self.write_auth(&cred)
    }

    pub fn read_auth(&self) -> Result<WebAuthFile, String> {
        let s = fs::read_to_string(&self.auth_file).map_err(|e| e.to_string())?;
        serde_json::from_str(&s).map_err(|e| e.to_string())
    }

    pub fn write_auth(&self, cred: &WebAuthFile) -> Result<(), String> {
        let tmp = self.auth_file.with_extension("tmp.json");
        let body = serde_json::to_string_pretty(cred).map_err(|e| e.to_string())?;
        fs::write(&tmp, body).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.auth_file).map_err(|e| e.to_string())
    }

    pub fn new_captcha(&self) -> (String, String) {
        let mut cap = self.captcha.lock().unwrap();
        let now = Instant::now();
        cap.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(CAPTCHA_SECS + 60));

        let id = Uuid::new_v4().to_string();
        let a = rand::random::<u8>() % 9 + 1;
        let b = rand::random::<u8>() % 9 + 1;
        let (a, b) = (a.max(b), a.min(b));
        let ops = ["+", "-", "*"];
        let op = ops.choose(&mut rand::thread_rng()).copied().unwrap();
        let ans = match op {
            "+" => a as i32 + b as i32,
            "-" => a as i32 - b as i32,
            "*" => a as i32 * b as i32,
            _ => unreachable!(),
        };
        let question = format!("{} {} {} = ?", a, op, b);
        cap.insert(id.clone(), (ans, Instant::now()));
        (id, question)
    }

    pub fn verify_captcha(&self, id: &str, answer_str: &str) -> bool {
        let Ok(ans) = answer_str.trim().parse::<i32>() else {
            return false;
        };
        let mut cap = self.captcha.lock().unwrap();
        if let Some((expect, t)) = cap.remove(id) {
            if t.elapsed() > Duration::from_secs(CAPTCHA_SECS) {
                return false;
            }
            return ans == expect;
        }
        false
    }

    pub fn login(&self, user: &str, pass: &str) -> Result<String, LoginFail> {
        let cred = self.read_auth().map_err(|e| {
            eprintln!(
                "[web_auth] login: 读取认证文件失败: {} (path={})",
                e,
                self.auth_file.display()
            );
            LoginFail::AuthFile
        })?;
        if user != cred.username {
            eprintln!(
                "[web_auth] login: 用户名不匹配: input={:?} expected={:?}",
                user, cred.username
            );
            return Err(LoginFail::BadCredentials);
        }
        match verify(pass, &cred.password_hash) {
            Ok(true) => { /* 密码正确，继续 */ }
            Ok(false) => {
                eprintln!(
                    "[web_auth] login: bcrypt verify 返回 false（密码不匹配），pass_len={} hash_prefix={}",
                    pass.len(),
                    &cred.password_hash[..cred.password_hash.len().min(20)]
                );
                return Err(LoginFail::BadCredentials);
            }
            Err(e) => {
                eprintln!(
                    "[web_auth] login: bcrypt verify 返回 Err: {} hash_prefix={}",
                    e,
                    &cred.password_hash[..cred.password_hash.len().min(20)]
                );
                return Err(LoginFail::BadCredentials);
            }
        }
        let tok = Uuid::new_v4().to_string();
        let mut sess = self.sessions.lock().unwrap();
        let now = Instant::now();
        sess.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(SESSION_SECS + 3600));
        if sess.len() >= MAX_SESSIONS {
            if let Some(oldest) = sess
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(tok, _)| tok.clone())
            {
                sess.remove(&oldest);
            }
        }
        sess.insert(tok.clone(), (user.to_string(), Instant::now()));
        Ok(tok)
    }

    pub fn login_allowed(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        let mut failures = self.login_failures.lock().unwrap();
        failures.retain(|_, v| {
            v.locked_until.map(|t| t > now).unwrap_or(false)
                || now.duration_since(v.window_start) < Duration::from_secs(LOGIN_WINDOW_SECS)
        });
        if let Some(v) = failures.get(key) {
            if let Some(until) = v.locked_until {
                if until > now {
                    return Err(until.duration_since(now));
                }
            }
        }
        Ok(())
    }

    pub fn record_login_failure(&self, key: &str) {
        let now = Instant::now();
        let mut failures = self.login_failures.lock().unwrap();
        let entry = failures.entry(key.to_string()).or_insert(LoginThrottle {
            failures: 0,
            window_start: now,
            locked_until: None,
        });
        if now.duration_since(entry.window_start) >= Duration::from_secs(LOGIN_WINDOW_SECS) {
            entry.failures = 0;
            entry.window_start = now;
            entry.locked_until = None;
        }
        entry.failures += 1;
        if entry.failures >= LOGIN_MAX_FAILURES {
            entry.locked_until = Some(now + Duration::from_secs(LOGIN_LOCK_SECS));
        }
    }

    pub fn record_login_success(&self, key: &str) {
        self.login_failures.lock().unwrap().remove(key);
    }

    pub fn session_username(&self, token: Option<&str>) -> Option<String> {
        let t = token?;
        let mut sess = self.sessions.lock().unwrap();
        if let Some((user, t0)) = sess.get(t) {
            if t0.elapsed() > Duration::from_secs(SESSION_SECS) {
                sess.remove(t);
                return None;
            }
            return Some(user.clone());
        }
        None
    }

    pub fn logout_token(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn insert_test_session(&self, token: &str, user: &str, age_secs: u64) {
        self.sessions.lock().unwrap().insert(
            token.to_string(),
            (
                user.to_string(),
                Instant::now() - Duration::from_secs(age_secs),
            ),
        );
    }

    pub fn change_password(&self, old_p: &str, new_p: &str) -> Result<(), String> {
        if new_p.len() < 8 {
            return Err("新密码至少 8 个字符".into());
        }
        let mut cred = self.read_auth()?;
        if !verify(old_p, &cred.password_hash).unwrap_or(false) {
            return Err("原密码不正确".into());
        }
        cred.password_hash = hash(new_p, DEFAULT_COST).map_err(|e| e.to_string())?;
        self.write_auth(&cred)?;
        self.sessions.lock().unwrap().clear();
        Ok(())
    }

    pub fn allow_iscsi_exec(&self) -> bool {
        self.read_auth()
            .map(|c| c.allow_iscsi_exec)
            .unwrap_or(false)
    }

    pub fn set_allow_iscsi_exec(&self, allow: bool) -> Result<(), String> {
        let mut cred = self.read_auth()?;
        cred.allow_iscsi_exec = allow;
        self.write_auth(&cred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_auth_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "vtladm_web_auth_{}_{}_{}",
            label,
            std::process::id(),
            rand::random::<u32>()
        ))
    }

    fn fresh_state(label: &str) -> WebState {
        let p = tmp_auth_path(label);
        let _ = fs::remove_file(&p);
        let st = WebState::new(p);
        st.init_auth_file().unwrap();
        st
    }

    /// 与 `new_captcha` 的 `format!("{} {} {} = ?", …)` 一致。
    fn captcha_expected_answer(question: &str) -> i32 {
        let parts: Vec<&str> = question.split_whitespace().collect();
        assert_eq!(parts.len(), 5, "expected `a op b = ?`, got {:?}", parts);
        let a: i32 = parts[0].parse().expect("a");
        let op = parts[1];
        let b: i32 = parts[2].parse().expect("b");
        assert_eq!(parts[3], "=");
        match op {
            "+" => a + b,
            "-" => a - b,
            "*" => a * b,
            _ => panic!("unknown op {:?}", op),
        }
    }

    #[test]
    fn test_web_auth_init_creates_json_with_defaults() {
        let st = fresh_state("init");
        let cred = st.read_auth().unwrap();
        assert_eq!(cred.username, DEFAULT_WEB_USER);
        assert!(cred.password_hash.starts_with("$2"));
        assert!(!cred.allow_iscsi_exec);
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_login_default_password_and_session() {
        let st = fresh_state("login");
        let tok = st
            .login(DEFAULT_WEB_USER, DEFAULT_WEB_PASSWORD)
            .expect("login");
        assert_eq!(
            st.session_username(Some(tok.as_str())).as_deref(),
            Some(DEFAULT_WEB_USER)
        );
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_set_allow_iscsi_exec_roundtrip() {
        let st = fresh_state("iscsi_allow");
        assert!(!st.allow_iscsi_exec());
        st.set_allow_iscsi_exec(true).unwrap();
        assert!(st.allow_iscsi_exec());
        let cred = st.read_auth().unwrap();
        assert!(cred.allow_iscsi_exec);
        st.set_allow_iscsi_exec(false).unwrap();
        assert!(!st.allow_iscsi_exec());
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_login_wrong_password() {
        let st = fresh_state("badpw");
        assert_eq!(
            st.login(DEFAULT_WEB_USER, "wrong-password-here"),
            Err(LoginFail::BadCredentials)
        );
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_captcha_correct_answer_accepted() {
        let st = fresh_state("cap_ok");
        for _ in 0..24 {
            let (id, q) = st.new_captcha();
            let ans = captcha_expected_answer(&q);
            assert!(st.verify_captcha(&id, &ans.to_string()), "question={}", q);
        }
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_captcha_wrong_answer_rejected() {
        let st = fresh_state("cap_bad");
        let (id, _q) = st.new_captcha();
        assert!(!st.verify_captcha(&id, "999999"));
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_captcha_double_verify_consumes_entry() {
        let st = fresh_state("cap_twice");
        let (id, q) = st.new_captcha();
        let ans = captcha_expected_answer(&q);
        assert!(st.verify_captcha(&id, &ans.to_string()));
        assert!(!st.verify_captcha(&id, &ans.to_string()));
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_change_password_roundtrip() {
        let st = fresh_state("chgpw");
        let old_tok = st
            .login(DEFAULT_WEB_USER, DEFAULT_WEB_PASSWORD)
            .expect("old pw");
        st.change_password(DEFAULT_WEB_PASSWORD, "newpass-8chars-min")
            .unwrap();
        assert!(st.session_username(Some(old_tok.as_str())).is_none());
        assert_eq!(
            st.login(DEFAULT_WEB_USER, DEFAULT_WEB_PASSWORD),
            Err(LoginFail::BadCredentials)
        );
        let tok = st
            .login(DEFAULT_WEB_USER, "newpass-8chars-min")
            .expect("new pw");
        assert!(st.session_username(Some(tok.as_str())).is_some());
        assert!(st.change_password("newpass-8chars-min", "short").is_err());
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_login_rate_limit_and_clear() {
        let st = fresh_state("rate");
        let key = "203.0.113.10";
        for _ in 0..5 {
            assert!(st.login_allowed(key).is_ok());
            st.record_login_failure(key);
        }
        assert!(st.login_allowed(key).is_err());
        st.record_login_success(key);
        assert!(st.login_allowed(key).is_ok());
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_session_cap() {
        let st = fresh_state("session_cap");
        st.insert_test_session("oldest", DEFAULT_WEB_USER, 1_000);
        for i in 1..MAX_SESSIONS {
            st.insert_test_session(&format!("tok{i}"), DEFAULT_WEB_USER, 10);
        }
        assert_eq!(st.session_count(), MAX_SESSIONS);
        let tok = st.login(DEFAULT_WEB_USER, DEFAULT_WEB_PASSWORD).unwrap();
        assert!(st.session_count() <= 128);
        assert!(st.session_username(Some("oldest")).is_none());
        assert!(st.session_username(Some(tok.as_str())).is_some());
        let _ = fs::remove_file(&st.auth_file);
    }

    #[test]
    fn test_web_auth_wrong_username_rejected() {
        let st = fresh_state("user");
        assert_eq!(
            st.login("not-admin", DEFAULT_WEB_PASSWORD),
            Err(LoginFail::BadCredentials)
        );
        let _ = fs::remove_file(&st.auth_file);
    }
}
