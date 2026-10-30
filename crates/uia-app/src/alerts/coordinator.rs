// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The alert coordinator: holds alerts while the agent is busy, speaks them
//! one at a time, sends the rest to the card, and closes a session only if it
//! opened that session itself.
//!
//! It owns no clock and does no I/O of its own: the app passes the time in
//! and performs every effect through [`AlertEnv`], so the whole machine is
//! driven deterministically in tests.
//!
//! In the real engine a typed turn and a voice turn look the same from
//! `State` alone (`Listening -> Speaking -> Listening`, with `ToolRunning ->
//! Thinking` in between when she calls a tool). So the coordinator counts a
//! busy state as the reply to its own turn only while that turn is
//! outstanding; any busy state at another time, and any barge-in
//! (`Interrupting`), is the user's, and hands the session over.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uia_core::session::State;
use uia_mcp::alert::Alert;

use super::{Route, TextTurns, UiState, route, spoken_turn_text};

/// What the coordinator needs from the app. Object-safe so the app can hand
/// over one `Arc<dyn AlertEnv>`.
pub trait AlertEnv: Send + Sync {
    /// The main window as the user sees it right now.
    fn ui_state(&self) -> UiState;
    fn text_turns(&self) -> TextTurns;
    fn show_card(&self, alert: Alert);
    /// Ask the session to open. The outcome arrives later through `on_state`.
    fn connect(&self);
    fn disconnect(&self);
    /// Submit a typed turn. `Err` means it was not accepted; the reason does
    /// not change what happens next (the alert goes to the card), so it
    /// carries none.
    #[allow(clippy::result_unit_err)]
    fn speak(&self, text: String) -> Result<(), ()>;
}

struct Held {
    alert: Alert,
    arrived: Instant,
}

/// Where the coordinator's own typed turn is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// No turn of ours is outstanding.
    None,
    /// Submitted; she has not started on it yet.
    Sent,
    /// She is on it (`Thinking`, `ToolRunning` or `Speaking` seen). The next
    /// `Listening` ends the reply.
    Answering,
}

pub struct Coordinator {
    env: Arc<dyn AlertEnv>,
    hold_timeout: Duration,
    session: State,
    held: VecDeque<Held>,
    turn: Turn,
    /// The coordinator opened the current session and the user has not
    /// taken it over, so it closes the session once it is done.
    opened_by_us: bool,
    /// `connect` was requested and no `Idle` has been seen since.
    connect_requested: bool,
}

impl Coordinator {
    pub fn new(env: Arc<dyn AlertEnv>, hold_timeout: Duration) -> Self {
        Self {
            env,
            hold_timeout,
            session: State::Idle,
            held: VecDeque::new(),
            turn: Turn::None,
            opened_by_us: false,
            connect_requested: false,
        }
    }

    /// A new alert. `show` off is filtered out by the caller before this.
    pub fn on_alert(&mut self, alert: Alert, wake_agent: bool, now: Instant) {
        let ui = self.env.ui_state();
        match route(wake_agent, ui, self.session, self.env.text_turns()) {
            Route::Card => self.env.show_card(alert),
            Route::SpeakNow => {
                self.hold(alert, now);
                self.speak_next();
            }
            Route::OpenSessionAndSpeak => {
                self.hold(alert, now);
                if !self.connect_requested {
                    self.connect_requested = true;
                    self.opened_by_us = true;
                    self.env.connect();
                }
            }
            Route::Hold => self.hold(alert, now),
        }
    }

    /// The session moved to `state`.
    pub fn on_state(&mut self, state: State, _now: Instant) {
        let previous = std::mem::replace(&mut self.session, state);
        match state {
            // Still idle: a connect just requested has not been answered yet.
            State::Idle if previous == State::Idle => {}
            State::Idle => {
                self.turn = Turn::None;
                self.opened_by_us = false;
                self.connect_requested = false;
                // Nothing will bring this session back to speak them.
                while let Some(held) = self.held.pop_front() {
                    self.env.show_card(held.alert);
                }
            }
            // A reconnect drops whatever turn was in flight.
            State::Connecting => self.turn = Turn::None,
            State::Listening => {
                match self.turn {
                    Turn::Sent => return,
                    Turn::Answering => self.turn = Turn::None,
                    Turn::None => {}
                }
                self.speak_next();
                self.close_if_done();
            }
            State::Thinking | State::ToolRunning | State::Speaking => match self.turn {
                Turn::Sent | Turn::Answering => self.turn = Turn::Answering,
                Turn::None => self.opened_by_us = false,
            },
            State::Interrupting => {
                self.opened_by_us = false;
                if self.turn == Turn::Sent {
                    self.turn = Turn::Answering;
                }
            }
        }
    }

    /// Time passed: alerts held longer than the hold timeout go to the card.
    pub fn on_tick(&mut self, now: Instant) {
        let timeout = self.hold_timeout;
        let (expired, kept): (VecDeque<Held>, VecDeque<Held>) = self
            .held
            .drain(..)
            .partition(|held| now.saturating_duration_since(held.arrived) >= timeout);
        self.held = kept;
        for held in expired {
            self.env.show_card(held.alert);
        }
        self.close_if_done();
    }

    /// Queue an alert. A newer one for the same `(server, id)` takes the
    /// older one's place, with its own arrival time.
    fn hold(&mut self, alert: Alert, now: Instant) {
        let same = self.held.iter_mut().find(|held| {
            alert.id.is_some() && held.alert.server == alert.server && held.alert.id == alert.id
        });
        match same {
            Some(held) => {
                held.alert = alert;
                held.arrived = now;
            }
            None => self.held.push_back(Held {
                alert,
                arrived: now,
            }),
        }
    }

    /// Speak the oldest held alert if she is free: re-checks the window first
    /// and sends anything that cannot be spoken to the card.
    fn speak_next(&mut self) {
        if self.turn != Turn::None || self.session != State::Listening {
            return;
        }
        while let Some(held) = self.held.pop_front() {
            if self.env.ui_state() != UiState::Visible {
                self.env.show_card(held.alert);
                continue;
            }
            let text = spoken_turn_text(&held.alert.server, &held.alert);
            match self.env.speak(text) {
                Ok(()) => {
                    self.turn = Turn::Sent;
                    return;
                }
                Err(()) => self.env.show_card(held.alert),
            }
        }
    }

    /// Close a session the coordinator opened once it has nothing left to say.
    fn close_if_done(&mut self) {
        if self.opened_by_us && self.turn == Turn::None && self.held.is_empty() {
            self.opened_by_us = false;
            self.env.disconnect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const HOLD: Duration = Duration::from_secs(30);

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Card(String),
        Connect,
        Disconnect,
        Speak(String),
    }

    struct FakeEnv {
        ui: Mutex<UiState>,
        text_turns: TextTurns,
        speak_fails: Mutex<bool>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeEnv {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                ui: Mutex::new(UiState::Visible),
                text_turns: TextTurns::Supported,
                speak_fails: Mutex::new(false),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn set_ui(&self, ui: UiState) {
            *self.ui.lock().unwrap() = ui;
        }

        fn fail_speaks(&self, fail: bool) {
            *self.speak_fails.lock().unwrap() = fail;
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl AlertEnv for FakeEnv {
        fn ui_state(&self) -> UiState {
            *self.ui.lock().unwrap()
        }
        fn text_turns(&self) -> TextTurns {
            self.text_turns
        }
        fn show_card(&self, alert: Alert) {
            self.record(Call::Card(alert.summary));
        }
        fn connect(&self) {
            self.record(Call::Connect);
        }
        fn disconnect(&self) {
            self.record(Call::Disconnect);
        }
        fn speak(&self, text: String) -> Result<(), ()> {
            self.record(Call::Speak(text));
            if *self.speak_fails.lock().unwrap() {
                Err(())
            } else {
                Ok(())
            }
        }
    }

    fn alert(id: Option<&str>, stage: &str, summary: &str) -> Alert {
        Alert {
            server: "calendar".into(),
            title: None,
            summary: summary.into(),
            details: None,
            kind: None,
            id: id.map(Into::into),
            stage: Some(stage.into()),
            created_at: None,
            action: None,
        }
    }

    fn spoken(a: &Alert) -> Call {
        Call::Speak(spoken_turn_text(&a.server, a))
    }

    fn card(a: &Alert) -> Call {
        Call::Card(a.summary.clone())
    }

    /// A coordinator whose session is already open and in `state`.
    fn open_in(env: &Arc<FakeEnv>, state: State, t0: Instant) -> Coordinator {
        let mut c = Coordinator::new(env.clone(), HOLD);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0);
        if state != State::Listening {
            c.on_state(state, t0);
        }
        c
    }

    /// The engine answering a typed turn: she speaks, then waits again.
    fn reply(c: &mut Coordinator, now: Instant) {
        c.on_state(State::Speaking, now);
        c.on_state(State::Listening, now);
    }

    #[test]
    fn a_held_alert_is_spoken_when_the_session_returns_to_listening() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Speaking, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        assert_eq!(env.calls(), vec![], "held while she is speaking");

        c.on_state(State::Listening, t0 + Duration::from_secs(2));
        assert_eq!(env.calls(), vec![spoken(&a)]);
    }

    #[test]
    fn a_held_alert_goes_to_the_card_after_30_seconds() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Thinking, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_tick(t0 + Duration::from_secs(29));
        assert_eq!(env.calls(), vec![], "still inside the hold window");

        c.on_tick(t0 + Duration::from_secs(30));
        assert_eq!(env.calls(), vec![card(&a)]);

        c.on_state(State::Listening, t0 + Duration::from_secs(31));
        assert_eq!(env.calls(), vec![card(&a)], "a carded alert is not spoken");
    }

    #[test]
    fn a_newer_stage_for_the_same_id_replaces_a_held_older_one() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Speaking, t0);
        let t5 = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        let other = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");
        let t2 = alert(Some("evt-1"), "T-2", "Standup in 2 minutes");

        c.on_alert(t5, true, t0);
        c.on_alert(other.clone(), true, t0);
        c.on_alert(t2.clone(), true, t0 + Duration::from_secs(20));

        // The replacement keeps its place in the queue and is spoken first.
        c.on_state(State::Listening, t0 + Duration::from_secs(21));
        assert_eq!(env.calls(), vec![spoken(&t2)]);

        // Its 30 s runs from its own arrival, so it outlives the alert behind it.
        let mut c = open_in(&env, State::Speaking, t0);
        let env_calls_before = env.calls().len();
        c.on_alert(alert(Some("evt-1"), "T-5", "old"), true, t0);
        c.on_alert(other.clone(), true, t0);
        c.on_alert(t2.clone(), true, t0 + Duration::from_secs(20));
        c.on_tick(t0 + Duration::from_secs(30));
        assert_eq!(env.calls()[env_calls_before..], [card(&other)]);
        c.on_tick(t0 + Duration::from_secs(50));
        assert_eq!(env.calls()[env_calls_before..], [card(&other), card(&t2)]);
    }

    #[test]
    fn alerts_without_an_id_never_replace_each_other() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Speaking, t0);
        let first = alert(None, "T-5", "First");
        let second = alert(None, "T-2", "Second");

        c.on_alert(first.clone(), true, t0);
        c.on_alert(second.clone(), true, t0);
        c.on_tick(t0 + HOLD);
        assert_eq!(env.calls(), vec![card(&first), card(&second)]);
    }

    #[test]
    fn held_alerts_are_spoken_one_at_a_time_each_after_the_previous_reply() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Speaking, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        let b = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_alert(b.clone(), true, t0);
        c.on_state(State::Listening, t0);
        assert_eq!(env.calls(), vec![spoken(&a)]);

        // Still Listening right after the typed turn: no second speak yet.
        c.on_state(State::Listening, t0);
        assert_eq!(env.calls(), vec![spoken(&a)]);

        reply(&mut c, t0 + Duration::from_secs(3));
        assert_eq!(env.calls(), vec![spoken(&a), spoken(&b)]);
    }

    #[test]
    fn an_alert_arriving_while_the_last_spoken_one_awaits_its_reply_is_held() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Listening, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        let b = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_alert(b.clone(), true, t0);
        assert_eq!(env.calls(), vec![spoken(&a)]);

        reply(&mut c, t0 + Duration::from_secs(3));
        assert_eq!(env.calls(), vec![spoken(&a), spoken(&b)]);
    }

    #[test]
    fn the_window_is_rechecked_at_speaking_time_and_a_hidden_window_shows_the_card() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Speaking, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        env.set_ui(UiState::Hidden);
        c.on_state(State::Listening, t0);
        assert_eq!(env.calls(), vec![card(&a)]);

        let b = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");
        env.set_ui(UiState::Visible);
        c.on_state(State::Speaking, t0);
        c.on_alert(b.clone(), true, t0);
        env.set_ui(UiState::Minimised);
        c.on_state(State::Listening, t0);
        assert_eq!(env.calls(), vec![card(&a), card(&b)]);
    }

    #[test]
    fn an_alert_that_is_spoken_does_not_also_show_a_card() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Listening, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        reply(&mut c, t0 + Duration::from_secs(2));
        c.on_tick(t0 + Duration::from_secs(60));
        assert_eq!(env.calls(), vec![spoken(&a)]);
    }

    #[test]
    fn a_session_the_coordinator_opened_is_closed_after_the_reply_to_its_own_turn() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        assert_eq!(env.calls(), vec![Call::Connect]);

        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0 + Duration::from_secs(1));
        assert_eq!(env.calls(), vec![Call::Connect, spoken(&a)]);

        c.on_state(State::Speaking, t0 + Duration::from_secs(2));
        assert_eq!(env.calls(), vec![Call::Connect, spoken(&a)]);
        c.on_state(State::Listening, t0 + Duration::from_secs(4));
        assert_eq!(
            env.calls(),
            vec![Call::Connect, spoken(&a), Call::Disconnect]
        );
    }

    #[test]
    fn a_second_alert_while_opening_the_session_does_not_connect_twice() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        let b = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_alert(b.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0);
        reply(&mut c, t0);
        reply(&mut c, t0);
        assert_eq!(
            env.calls(),
            vec![Call::Connect, spoken(&a), spoken(&b), Call::Disconnect]
        );
    }

    #[test]
    fn an_alert_held_while_opening_the_session_times_out_to_the_card_and_closes_it() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        c.on_tick(t0 + HOLD);
        assert_eq!(env.calls(), vec![Call::Connect, card(&a), Call::Disconnect]);
    }

    #[test]
    fn an_idle_report_before_the_session_opens_keeps_the_alert_and_the_ownership() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Idle, t0);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0);
        reply(&mut c, t0);
        assert_eq!(
            env.calls(),
            vec![Call::Connect, spoken(&a), Call::Disconnect]
        );
    }

    #[test]
    fn a_session_that_fails_to_open_sends_its_alerts_to_the_card() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Idle, t0 + Duration::from_secs(1));
        assert_eq!(env.calls(), vec![Call::Connect, card(&a)]);
    }

    #[test]
    fn a_session_is_not_closed_when_the_user_starts_a_turn_first() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0);

        // She starts on the alert and the user talks over her: the session
        // is theirs now. Their own turns follow.
        c.on_state(State::Speaking, t0);
        c.on_state(State::Interrupting, t0);
        c.on_state(State::Listening, t0);
        reply(&mut c, t0);
        c.on_tick(t0 + Duration::from_secs(60));
        assert_eq!(env.calls(), vec![Call::Connect, spoken(&a)]);
    }

    #[test]
    fn a_turn_that_the_coordinator_did_not_send_hands_the_session_over() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        // A turn of the user's lands before the alert's could be sent.
        c.on_state(State::Thinking, t0);
        c.on_state(State::Speaking, t0);
        c.on_state(State::Listening, t0);
        reply(&mut c, t0);
        c.on_tick(t0 + Duration::from_secs(60));
        assert_eq!(env.calls(), vec![Call::Connect, spoken(&a)]);
    }

    #[test]
    fn a_tool_call_inside_the_reply_to_its_own_turn_still_closes_the_session() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0);
        c.on_state(State::ToolRunning, t0);
        c.on_state(State::Thinking, t0);
        c.on_state(State::Speaking, t0);
        assert_eq!(env.calls(), vec![Call::Connect, spoken(&a)]);
        c.on_state(State::Listening, t0);
        assert_eq!(
            env.calls(),
            vec![Call::Connect, spoken(&a), Call::Disconnect]
        );
    }

    #[test]
    fn a_session_that_was_already_open_is_never_closed() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Listening, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        let b = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        reply(&mut c, t0);
        c.on_state(State::Speaking, t0);
        c.on_alert(b.clone(), true, t0);
        env.set_ui(UiState::Hidden);
        c.on_state(State::Listening, t0);
        c.on_tick(t0 + Duration::from_secs(60));
        assert_eq!(env.calls(), vec![spoken(&a), card(&b)]);
    }

    #[test]
    fn a_failed_speak_falls_back_to_the_card() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Speaking, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");
        let b = alert(Some("evt-2"), "T-5", "Lunch in 5 minutes");

        c.on_alert(a.clone(), true, t0);
        c.on_alert(b.clone(), true, t0);
        env.fail_speaks(true);
        c.on_state(State::Listening, t0);
        assert_eq!(
            env.calls(),
            vec![spoken(&a), card(&a), spoken(&b), card(&b)],
            "each failure goes to the card and the next held one is tried"
        );
    }

    #[test]
    fn a_failed_speak_in_a_session_the_coordinator_opened_closes_it() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = Coordinator::new(env.clone(), HOLD);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        env.fail_speaks(true);
        c.on_alert(a.clone(), true, t0);
        c.on_state(State::Connecting, t0);
        c.on_state(State::Listening, t0);
        assert_eq!(
            env.calls(),
            vec![Call::Connect, spoken(&a), card(&a), Call::Disconnect]
        );
    }

    #[test]
    fn wake_off_goes_straight_to_the_card() {
        let env = FakeEnv::new();
        let t0 = Instant::now();
        let mut c = open_in(&env, State::Listening, t0);
        let a = alert(Some("evt-1"), "T-5", "Standup in 5 minutes");

        c.on_alert(a.clone(), false, t0);
        assert_eq!(env.calls(), vec![card(&a)]);
    }
}
