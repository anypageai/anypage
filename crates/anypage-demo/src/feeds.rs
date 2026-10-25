//! Push-stream producers on their own threads, at irregular rates with bursts
//! and pauses, so the graph's batching and backpressure get exercised.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_core::StreamProducer;

#[derive(Clone)]
pub struct Tick {
    pub t: f32,
    pub v: f32,
    pub produced: Instant,
}

#[derive(Clone)]
pub struct Token {
    pub text: String,
    pub produced: Instant,
}

/// xorshift64*: no dependency needed for demo randomness.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo).max(1)
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

pub fn spawn_ticks(p: StreamProducer<Tick>, on: Arc<AtomicBool>, quit: Arc<AtomicBool>, start: Instant) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("feed-ticks".into())
        .spawn(move || {
            let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
            let mut v = 100.0f32;
            let mut drift = 0.0f32;
            while !quit.load(Ordering::Relaxed) {
                if !on.load(Ordering::Relaxed) {
                    sleep_ms(50);
                    continue;
                }
                // occasionally a burst of ticks with no gap: many items, one commit
                let burst = if rng.range(0, 100) < 3 { rng.range(20, 60) } else { 1 };
                for _ in 0..burst {
                    drift = drift * 0.97 + (rng.f32() - 0.5) * 0.15;
                    v = (v + drift + (rng.f32() - 0.5) * 0.8).max(1.0);
                    p.push(Tick { t: start.elapsed().as_secs_f32(), v, produced: Instant::now() });
                }
                sleep_ms(rng.range(6, 45));
            }
        })
        .expect("spawn ticks")
}

pub const CORPUS: &[&str] = &[
    "AnyPage 把靜態、流式、非同步與操作中的內容放進同一個頁面。",
    "每一種內容都是計算圖上的一個格，差別只在它什麼時候會變。",
    "The scheduler fills each frame's budget: operations first, then visible streams, then async results.",
    "沒有變化的時候，引擎是 0 fps，不耗電。",
    "推式串流在每一幀合批進同一次 commit，不會一筆資料觸發一次重排。",
    "Tiles that leave the viewport are cancelled before they are rasterised.",
    "下載的頁面只描述計算，重活由編進 App 的原生算子與 GPU 算子完成。",
    "這段文字本身就是一條模擬的逐字輸出串流，速率不固定，有時連發，有時停頓。",
    "Drag the hue slider: the fractal tiles, the chart, the GPU effect and the animation all depend on the same input cell.",
    "離開視口的面板不會被求值；串流仍然進入圖，但沒有人要它的畫面。",
];

pub fn spawn_tokens(p: StreamProducer<Token>, on: Arc<AtomicBool>, quit: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("feed-tokens".into())
        .spawn(move || {
            let mut rng = Rng::new(0xD1B5_4A32_D192_ED03);
            let mut k = 0usize;
            while !quit.load(Ordering::Relaxed) {
                if !on.load(Ordering::Relaxed) {
                    sleep_ms(50);
                    continue;
                }
                let sentence: Vec<char> = CORPUS[k % CORPUS.len()].chars().collect();
                k += 1;
                let mut i = 0;
                while i < sentence.len() && !quit.load(Ordering::Relaxed) {
                    if !on.load(Ordering::Relaxed) {
                        sleep_ms(50);
                        continue;
                    }
                    let n = rng.range(1, 5) as usize;
                    let chunk: String = sentence[i..(i + n).min(sentence.len())].iter().collect();
                    i += n;
                    p.push(Token { text: chunk, produced: Instant::now() });
                    sleep_ms(rng.range(20, 110));
                }
                let end = if rng.range(0, 3) == 0 { "\n" } else { " " };
                p.push(Token { text: end.to_string(), produced: Instant::now() });
                sleep_ms(rng.range(200, 900));
            }
        })
        .expect("spawn tokens")
}
