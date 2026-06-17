use std::time::{Duration, Instant};

use kdam::{tqdm, Bar, BarExt};

use crate::pipeline::TileStats;

const PROGRESS_SCALE: usize = 10_000;
const PMTILES_GENERATE_WEIGHT: usize = 9_000;
const MBTILES_GENERATE_WEIGHT: usize = 10_000;

pub(crate) struct SingleProgress {
    bar: Bar,
    started_at: Instant,
    generation_total: u64,
    generation_done: u64,
    generation_weight: usize,
    build_total: u64,
    build_done: u64,
    build_weight: usize,
}

impl SingleProgress {
    pub(crate) fn pmtiles(generation_total: u64) -> Self {
        Self::new(generation_total, PMTILES_GENERATE_WEIGHT, PROGRESS_SCALE - PMTILES_GENERATE_WEIGHT)
    }

    pub(crate) fn mbtiles(generation_total: u64) -> Self {
        Self::new(generation_total, MBTILES_GENERATE_WEIGHT, 0)
    }

    fn new(generation_total: u64, generation_weight: usize, build_weight: usize) -> Self {
        let mut bar = tqdm!(
            total = PROGRESS_SCALE,
            ncols = 16,
            dynamic_ncols = false,
            mininterval = 0.1,
            leave = false,
            bar_format = "{desc suffix=' '}|{animation}| {percentage:.1}% {postfix suffix=' '}",
            unit = "steps"
        );
        bar.set_description("[init]");
        Self {
            bar,
            started_at: Instant::now(),
            generation_total: generation_total.max(1),
            generation_done: 0,
            generation_weight,
            build_total: 1,
            build_done: 0,
            build_weight,
        }
    }

    pub(crate) fn set_generate_stage(&mut self, z: u8, stage_done: u64, stage_total: usize, stats: &TileStats) {
        self.set_stage(format!("[z{} {}/{}]", z, stage_done, stage_total), stats);
    }

    pub(crate) fn advance_generation(
        &mut self,
        completed: u64,
        z: u8,
        stage_done: u64,
        stage_total: usize,
        stats: &TileStats,
    ) {
        self.generation_done = self.generation_done.saturating_add(completed).min(self.generation_total);
        self.set_generate_stage(z, stage_done, stage_total, stats);
        self.refresh_position();
    }

    pub(crate) fn finish_generation(&mut self, stats: &TileStats) {
        self.generation_done = self.generation_total;
        self.set_stage("[generate done]", stats);
        self.refresh_position();
    }

    pub(crate) fn start_build(&mut self, total: u64, stats: &TileStats) {
        self.build_total = total.max(1);
        self.build_done = 0;
        self.set_stage(format!("[pmtiles 0/{}]", total), stats);
        self.refresh_position();
    }

    pub(crate) fn advance_build(&mut self, n: u64, stats: &TileStats) {
        self.build_done = self.build_done.saturating_add(n).min(self.build_total);
        self.set_stage(format!("[pmtiles {}/{}]", self.build_done, self.build_total), stats);
        self.refresh_position();
    }

    pub(crate) fn finish(&mut self, stats: &TileStats) {
        self.generation_done = self.generation_total;
        self.build_done = self.build_total;
        self.set_stage("[done]", stats);
        let _ = self.bar.update_to(PROGRESS_SCALE);
        let _ = self.bar.refresh();
        eprintln!();
    }

    pub(crate) fn write_warning<T: Into<String>>(&mut self, message: T) {
        let _ = self.bar.write(message);
    }

    fn set_stage<T: Into<String>>(&mut self, stage: T, stats: &TileStats) {
        self.bar.set_description(stage);
        self.bar.set_postfix(format!(
            "written {} | empty {} | pruned {} | filled {} | elapsed {} | eta {} | speed {}/s",
            format_count(stats.written),
            format_count(stats.empty),
            format_count(stats.pruned),
            format_count(stats.filled),
            format_duration(self.elapsed()),
            format_duration(self.eta()),
            format_count(self.speed(stats))
        ));
    }

    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn eta(&self) -> Duration {
        let position = self.position();
        if position == 0 {
            return Duration::ZERO;
        }
        let elapsed = self.elapsed().as_secs_f64();
        let remaining = elapsed * (PROGRESS_SCALE.saturating_sub(position) as f64 / position as f64);
        Duration::from_secs_f64(remaining.max(0.0))
    }

    fn speed(&self, stats: &TileStats) -> u64 {
        let elapsed = self.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return 0;
        }
        (stats.checked as f64 / elapsed).round() as u64
    }

    fn refresh_position(&mut self) {
        let position = self.position();
        let _ = self.bar.update_to(position);
    }

    fn position(&self) -> usize {
        let generation_pos = weighted_position(self.generation_done, self.generation_total, self.generation_weight);
        let build_pos = weighted_position(self.build_done, self.build_total, self.build_weight);
        (generation_pos + build_pos).min(PROGRESS_SCALE)
    }
}

fn weighted_position(done: u64, total: u64, weight: usize) -> usize {
    if weight == 0 {
        return 0;
    }
    ((done.min(total) as u128 * weight as u128) / total.max(1) as u128) as usize
}

fn format_count(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{}h{:02}m{:02}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m{:02}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}
