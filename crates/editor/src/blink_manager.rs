use gpui::Context;
use settings::SettingsStore;
use std::time::Duration;
use ui::App;

/// Each blink is a full window redraw, so stop blinking (cursor shown) after this long without input.
const BLINK_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct BlinkManager {
    blink_interval: Duration,
    blink_epoch: usize,
    blinks_since_activity: u32,
    /// Whether the blinking is paused.
    blinking_paused: bool,
    /// Whether the cursor should be visibly rendered or not.
    visible: bool,
    /// Whether the blinking is currently enabled.
    enabled: bool,
    /// Whether the blinking is enabled in the settings.
    blink_enabled_in_settings: fn(&App) -> bool,
}

impl BlinkManager {
    pub fn new(
        blink_interval: Duration,
        blink_enabled_in_settings: fn(&App) -> bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // Make sure we blink the cursors if the setting is re-enabled
        cx.observe_global::<SettingsStore>(move |this, cx| {
            this.blink_cursors(this.blink_epoch, cx)
        })
        .detach();

        Self {
            blink_interval,
            blink_epoch: 0,
            blinks_since_activity: 0,
            blinking_paused: false,
            visible: true,
            enabled: false,
            blink_enabled_in_settings,
        }
    }

    fn next_blink_epoch(&mut self) -> usize {
        self.blink_epoch += 1;
        self.blink_epoch
    }

    pub fn pause_blinking(&mut self, cx: &mut Context<Self>) {
        self.show_cursor(cx);
        self.blinks_since_activity = 0;

        let epoch = self.next_blink_epoch();
        let interval = Duration::from_millis(500);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            this.update(cx, |this, cx| this.resume_cursor_blinking(epoch, cx))
        })
        .detach();
    }

    fn resume_cursor_blinking(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if epoch == self.blink_epoch {
            self.blinking_paused = false;
            self.blink_cursors(epoch, cx);
        }
    }

    fn blink_cursors(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if (self.blink_enabled_in_settings)(cx) {
            if epoch == self.blink_epoch && self.enabled && !self.blinking_paused {
                if self.blinks_since_activity >= self.max_idle_blinks() {
                    self.show_cursor(cx);
                    return;
                }
                self.blinks_since_activity += 1;
                self.visible = !self.visible;
                cx.notify();

                let epoch = self.next_blink_epoch();
                let interval = self.blink_interval;
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(interval).await;
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| this.blink_cursors(epoch, cx));
                    }
                })
                .detach();
            }
        } else {
            self.show_cursor(cx);
        }
    }

    pub fn show_cursor(&mut self, cx: &mut Context<BlinkManager>) {
        if !self.visible {
            self.visible = true;
            cx.notify();
        }
    }

    /// Enable the blinking of the cursor.
    pub fn enable(&mut self, cx: &mut Context<Self>) {
        if self.enabled {
            return;
        }

        self.enabled = true;
        self.blinks_since_activity = 0;
        // Set cursors as invisible and start blinking: this causes cursors
        // to be visible during the next render.
        self.visible = false;
        self.blink_cursors(self.blink_epoch, cx);
    }

    /// Disable the blinking of the cursor.
    pub fn disable(&mut self, _cx: &mut Context<Self>) {
        self.visible = false;
        self.enabled = false;
    }

    fn max_idle_blinks(&self) -> u32 {
        (BLINK_IDLE_TIMEOUT.as_millis() / self.blink_interval.as_millis().max(1)) as u32
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    #[cfg(test)]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext};

    fn advance(blink_manager: &Entity<BlinkManager>, duration: Duration, cx: &mut TestAppContext) {
        let interval = blink_manager.read_with(cx, |this, _| this.blink_interval);
        let mut elapsed = Duration::ZERO;
        while elapsed < duration {
            cx.executor().advance_clock(interval);
            cx.run_until_parked();
            elapsed += interval;
        }
    }

    #[gpui::test]
    fn test_blinking_stops_after_idle_timeout(cx: &mut TestAppContext) {
        let blink_manager =
            cx.new(|cx| BlinkManager::new(Duration::from_millis(500), |_| true, cx));
        blink_manager.update(cx, |this, cx| this.enable(cx));

        advance(&blink_manager, BLINK_IDLE_TIMEOUT * 2, cx);
        let idle_epoch = blink_manager.read_with(cx, |this, _| {
            assert!(
                this.visible(),
                "cursor must stay visible once blinking stops"
            );
            this.blink_epoch
        });
        advance(&blink_manager, Duration::from_secs(5), cx);
        assert_eq!(
            blink_manager.read_with(cx, |this, _| this.blink_epoch),
            idle_epoch,
            "no blink timers should run while idle"
        );

        blink_manager.update(cx, |this, cx| this.pause_blinking(cx));
        advance(&blink_manager, Duration::from_secs(3), cx);
        assert!(
            blink_manager.read_with(cx, |this, _| this.blink_epoch) > idle_epoch + 1,
            "input should resume blinking"
        );
    }
}
