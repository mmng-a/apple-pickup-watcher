//! 日本時間で毎日繰り返す監視時間帯。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 開始時刻を含み、終了時刻を含まない。日付をまたぐ範囲も指定できる。
/// 0〜24時、および同じ時刻どうしの指定は終日監視になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MonitoringHours {
    pub start_hour: u8,
    pub end_hour: u8,
}

impl Default for MonitoringHours {
    fn default() -> Self {
        Self {
            start_hour: 0,
            end_hour: 24,
        }
    }
}

impl MonitoringHours {
    /// 入力可能な時刻かどうか。
    pub fn is_valid(self) -> bool {
        self.start_hour < 24 && self.end_hour <= 24
    }

    /// 現在が監視時間内かどうかと、次に時間帯を確認するまでの待ち時間。
    pub(crate) fn current(self) -> (bool, Duration) {
        self.at(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default())
    }

    fn at(self, unix: Duration) -> (bool, Duration) {
        let start = u64::from(self.start_hour % 24) * 3600;
        let end = u64::from(self.end_hour % 24) * 3600;
        if start == end {
            return (true, Duration::from_secs(86400));
        }
        let second = (unix.as_secs() % 86400 + 9 * 3600) % 86400;
        let active = if start < end {
            start <= second && second < end
        } else {
            second >= start || second < end
        };
        let boundary = if active { end } else { start };
        let wait = Duration::from_secs((boundary + 86400 - second) % 86400)
            .saturating_sub(Duration::from_nanos(u64::from(unix.subsec_nanos())));
        // スリープ復帰や時計の変更後も、長いタイマーを待たずに現在時刻を確認する。
        (active, wait.min(Duration::from_secs(30)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jst(hour: u64, minute: u64) -> Duration {
        Duration::from_secs(86400 + hour * 3600 + minute * 60 - 9 * 3600)
    }

    #[test]
    fn daily_windows_use_jst_and_exclude_the_end() {
        let hours = MonitoringHours {
            start_hour: 6,
            end_hour: 12,
        };
        for (hour, minute, active) in [(5, 59, false), (6, 0, true), (11, 59, true), (12, 0, false)]
        {
            assert_eq!(hours.at(jst(hour, minute)).0, active);
        }
        let overnight = MonitoringHours {
            start_hour: 15,
            end_hour: 14,
        };
        for hour in 0..24 {
            assert_eq!(overnight.at(jst(hour, 0)).0, hour != 14);
            assert!(MonitoringHours::default().at(jst(hour, 0)).0);
            assert!(
                MonitoringHours {
                    start_hour: 6,
                    end_hour: 6
                }
                .at(jst(hour, 0))
                .0
            );
        }
        assert!(
            !MonitoringHours {
                start_hour: 24,
                end_hour: 12
            }
            .is_valid()
        );
        assert!(
            !MonitoringHours {
                start_hour: 0,
                end_hour: 25
            }
            .is_valid()
        );
        assert_eq!(
            hours.at(jst(11, 59) + Duration::from_millis(59_500)).1,
            Duration::from_millis(500)
        );
    }
}
