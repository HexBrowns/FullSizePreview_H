//! 受け口の読み出しの間引き（本体の再生 fps を落とさないため）。
//!
//! 読み出し（`copy_image_resource` → `get_image_resource_data`）は本体の描画スレッドで行うので、
//! かかった時間がそのまま本体の 1 フレームに上乗せされる（FHD の実機で平均 2.5〜9ms・最大 21ms。4K はその数倍の見込み）。
//!
//! - **本体が再生に追いついているか**を、受け口が呼ばれる間隔で見る。再生バッファが埋まっていれば本体は時刻に合わせて待つので、
//!   間隔は 1 フレームの時間（1000 / (fps × 再生速度) ms）ちょうどになる。それより長ければ本体は追いついていない
//! - 追いついていない間は、読み出しの分（1 回の時間 ÷ N）が 1 フレームの `SHARE` を超えていれば、N フレームに 1 回へ 1 段ずつ広げる（`MAX_EVERY` まで）
//! - 追いついている状態が `CALM` 続いたら 1 段ずつ戻す（戻して追いつかなくなれば、また広げる）
//! - 本体が自分の重さだけで追いついていないとき（読み出しの分が小さいとき）は広げない。広げても本体は速くならない
//! - 届く画像が空（カメラ制御の対象に入っている）と分かった後は、`IDLE_INTERVAL` に 1 回だけ読む（直ったかを見るため）

use std::time::{Duration, Instant};

/// 追いついていないとき、読み出しに使ってよい 1 フレームの割合
pub const SHARE: f64 = 0.05;
/// 間引きの上限（N フレームに 1 回の N）。30fps なら 1 秒に 5 枚は映す
pub const MAX_EVERY: u32 = 6;
/// 呼ばれる間隔がフレームの時間のこの倍を超えたら、追いついていないとみる
const BEHIND_RATIO: f64 = 1.05;
/// 間隔の平均の重み（指数移動平均。おおむね直近 10 回）
const INTERVAL_WEIGHT: f64 = 0.1;
/// 読み出しの時間の平均の重み
const COST_WEIGHT: f64 = 0.2;
/// 判断を始めるまでに見る呼び出しの数（再生の始めは本体が先読みで詰めて描く）
const WARMUP_CALLS: u64 = 10;
/// 広げるときの間（平均が新しい間隔に馴染むのを待つ）
const STEP_INTERVAL: Duration = Duration::from_millis(500);
/// 追いついている状態がこれだけ続いたら 1 段戻す
const CALM: Duration = Duration::from_secs(3);
/// これより長い間隔は数えない（本体が止まっていた・シークした）
const MAX_SAMPLE_INTERVAL_MS: f64 = 1000.0;
/// 画像が空と分かった後に読む間隔
pub const IDLE_INTERVAL: Duration = Duration::from_millis(500);

/// 初めて間引いたときの記録（再生 1 回につき 1 回だけログに出す）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Notice {
    pub every: u32,
    pub cost_ms: f64,
    pub interval_ms: f64,
    pub period_ms: f64,
}

#[derive(Debug)]
pub struct Throttle {
    /// 1 フレームの時間（ms）
    period_ms: f64,
    /// N フレームに 1 回読む
    every: u32,
    /// 前に読んでから飛ばした回数
    since_read: u32,
    last_call: Option<Instant>,
    calls: u64,
    interval_ms: Option<f64>,
    cost_ms: Option<f64>,
    last_change: Option<Instant>,
    calm_since: Option<Instant>,
    idle: bool,
    last_read: Option<Instant>,
    /// 飛ばした回数（閉じたときの集計）
    pub skipped: u64,
    /// この再生で一番広げた N
    pub max_every: u32,
    notice: Option<Notice>,
    noticed: bool,
}

impl Throttle {
    /// `frames_per_sec` は fps × 再生速度
    pub fn new(frames_per_sec: f64) -> Self {
        let period_ms = if frames_per_sec > 0.0 { 1000.0 / frames_per_sec } else { 1000.0 / 30.0 };
        Self {
            period_ms,
            every: 1,
            since_read: 0,
            last_call: None,
            calls: 0,
            interval_ms: None,
            cost_ms: None,
            last_change: None,
            calm_since: None,
            idle: false,
            last_read: None,
            skipped: 0,
            max_every: 1,
            notice: None,
            noticed: false,
        }
    }

    #[cfg(test)]
    pub fn every(&self) -> u32 {
        self.every
    }

    /// 受け口が呼ばれるたびに呼ぶ。この回に読むなら true
    pub fn on_call(&mut self, now: Instant) -> bool {
        if let Some(prev) = self.last_call {
            let iv = now.saturating_duration_since(prev).as_secs_f64() * 1000.0;
            if iv <= MAX_SAMPLE_INTERVAL_MS {
                self.interval_ms = Some(match self.interval_ms {
                    Some(avg) => avg + (iv - avg) * INTERVAL_WEIGHT,
                    None => iv,
                });
            }
        }
        self.last_call = Some(now);
        self.calls += 1;
        self.adjust(now);

        let read = if self.idle {
            self.last_read.is_none_or(|t| now.saturating_duration_since(t) >= IDLE_INTERVAL)
        } else {
            self.since_read + 1 >= self.every
        };
        if !read {
            self.since_read += 1;
            self.skipped += 1;
        }
        read
    }

    /// 読んだら呼ぶ（受け口が別の理由で読んだときも）。`cost_ms` は読み出しにかかった時間
    pub fn on_read(&mut self, cost_ms: f64, now: Instant) {
        self.cost_ms = Some(match self.cost_ms {
            Some(avg) => avg + (cost_ms - avg) * COST_WEIGHT,
            None => cost_ms,
        });
        self.since_read = 0;
        self.last_read = Some(now);
    }

    /// 届く画像が空の間は `IDLE_INTERVAL` に 1 回だけ読む
    pub fn set_idle(&mut self, idle: bool) {
        self.idle = idle;
    }

    /// 初めて間引いたときの記録を 1 回だけ返す
    pub fn take_notice(&mut self) -> Option<Notice> {
        self.notice.take()
    }

    /// 本体が再生に追いついていないか
    pub fn behind(&self) -> bool {
        self.calls >= WARMUP_CALLS && self.interval_ms.is_some_and(|iv| iv > self.period_ms * BEHIND_RATIO)
    }

    fn adjust(&mut self, now: Instant) {
        let settled = self.last_change.is_none_or(|t| now.saturating_duration_since(t) >= STEP_INTERVAL);
        if self.behind() {
            self.calm_since = None;
            let share = self.cost_ms.unwrap_or(0.0) / self.every as f64;
            if settled && self.every < MAX_EVERY && share > self.period_ms * SHARE {
                self.every += 1;
                self.last_change = Some(now);
                self.max_every = self.max_every.max(self.every);
                if !self.noticed {
                    self.noticed = true;
                    self.notice = Some(Notice {
                        every: self.every,
                        cost_ms: self.cost_ms.unwrap_or(0.0),
                        interval_ms: self.interval_ms.unwrap_or(0.0),
                        period_ms: self.period_ms,
                    });
                }
            }
        } else {
            let calm_since = *self.calm_since.get_or_insert(now);
            let calm = now.saturating_duration_since(calm_since) >= CALM
                && self.last_change.is_none_or(|t| now.saturating_duration_since(t) >= CALM);
            if self.every > 1 && calm {
                self.every -= 1;
                self.last_change = Some(now);
                self.calm_since = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本体の 1 フレームの時間が `host_ms`、読み出しが `read_ms` のとき、`secs` 秒ぶん回す。
    /// 呼ばれる間隔は「本体の時間 ＋ この回に読んだら読み出しの時間」と、フレームの時間の長い方
    fn run(t: &mut Throttle, now: &mut Instant, period_ms: f64, host_ms: f64, read_ms: f64, secs: f64) -> (u64, u64) {
        let end = *now + Duration::from_secs_f64(secs);
        let (mut calls, mut reads) = (0, 0);
        while *now < end {
            calls += 1;
            let read = t.on_call(*now);
            let mut took = host_ms;
            if read {
                reads += 1;
                t.on_read(read_ms, *now);
                took += read_ms;
            }
            *now += Duration::from_secs_f64(took.max(period_ms) / 1000.0);
        }
        (calls, reads)
    }

    #[test]
    fn keeps_every_frame_while_host_keeps_up() {
        // 本体 10ms ＋ 読み出し 9ms でも 33.3ms に収まる → 間引かない
        let mut t = Throttle::new(30.0);
        let mut now = Instant::now();
        let (calls, reads) = run(&mut t, &mut now, 1000.0 / 30.0, 10.0, 9.0, 5.0);
        assert_eq!(calls, reads);
        assert_eq!((t.every(), t.skipped, t.max_every), (1, 0, 1));
        assert!(t.take_notice().is_none());
    }

    #[test]
    fn thins_out_when_readback_makes_host_fall_behind() {
        // 本体 30ms ＋ 読み出し 9ms = 39ms > 33.3ms → 広げて、本体が追いつく
        let period = 1000.0 / 30.0;
        let mut t = Throttle::new(30.0);
        let mut now = Instant::now();
        run(&mut t, &mut now, period, 30.0, 9.0, 3.0);
        assert!(t.every() >= 2, "every={}", t.every());
        let n = t.take_notice().expect("初めて間引いたことを 1 回だけ知らせる");
        assert_eq!(n.every, 2);
        assert!(t.take_notice().is_none());
        // 落ち着いた後の 10 秒: 平均の呼ばれる間隔がフレームの時間の 5% 以内
        let start = now;
        let (calls, _) = run(&mut t, &mut now, period, 30.0, 9.0, 10.0);
        let avg = (now - start).as_secs_f64() * 1000.0 / calls as f64;
        assert!(avg < period * 1.05, "平均 {avg:.1}ms");
        assert!(t.skipped > 0);
    }

    #[test]
    fn does_not_thin_out_when_host_is_slow_by_itself() {
        // 本体だけで 50ms。読み出しは 1ms（フレームの 3%）→ 広げても本体は速くならないので広げない
        let mut t = Throttle::new(30.0);
        let mut now = Instant::now();
        run(&mut t, &mut now, 1000.0 / 30.0, 50.0, 1.0, 5.0);
        assert_eq!(t.every(), 1);
    }

    #[test]
    fn caps_at_max_every_for_huge_readback() {
        // 4K で読み出し 40ms（フレームより長い）
        let mut t = Throttle::new(30.0);
        let mut now = Instant::now();
        run(&mut t, &mut now, 1000.0 / 30.0, 20.0, 40.0, 10.0);
        assert_eq!(t.every(), MAX_EVERY);
        assert_eq!(t.max_every, MAX_EVERY);
    }

    #[test]
    fn steps_back_when_host_catches_up() {
        let period = 1000.0 / 30.0;
        let mut t = Throttle::new(30.0);
        let mut now = Instant::now();
        run(&mut t, &mut now, period, 30.0, 9.0, 4.0);
        assert!(t.every() >= 2);
        // シーンが軽くなった（本体 5ms）→ 追いついている状態が続けば 1 に戻る
        run(&mut t, &mut now, period, 5.0, 9.0, 20.0);
        assert_eq!(t.every(), 1);
    }

    #[test]
    fn speed_changes_the_budget() {
        // 再生速度 200%（60 フレーム/秒）なら 1 フレーム 16.7ms。本体 10ms ＋ 読み出し 9ms で追いつかない
        let mut t = Throttle::new(60.0);
        let mut now = Instant::now();
        run(&mut t, &mut now, 1000.0 / 60.0, 10.0, 9.0, 3.0);
        assert!(t.every() >= 2);
    }

    #[test]
    fn idle_reads_twice_a_second() {
        let mut t = Throttle::new(30.0);
        t.set_idle(true);
        let mut now = Instant::now();
        let (calls, reads) = run(&mut t, &mut now, 1000.0 / 30.0, 1.0, 1.0, 2.0);
        assert!(calls >= 59);
        assert!((4..=5).contains(&reads), "reads={reads}");
        t.set_idle(false);
        let (calls, reads) = run(&mut t, &mut now, 1000.0 / 30.0, 1.0, 1.0, 1.0);
        assert_eq!(calls, reads);
    }
}
