use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// 规范化的思考等级；提供方各自通过配置里的 `effort_map` 映射到实际参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("未知思考等级 `{s}`，可选：low/medium/high/xhigh/max"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_roundtrip() {
        for e in Effort::ALL {
            assert_eq!(e.as_str().parse::<Effort>().unwrap(), e);
        }
        assert!("ultra".parse::<Effort>().is_err());
    }
}
