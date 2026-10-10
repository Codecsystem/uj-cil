use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Verdict for a test case or entire submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema)]
pub enum Verdict {
    Accepted,            // AC
    WrongAnswer,         // WA
    PresentationError,   // PE
    PartiallyCorrect,    // PC
    TimeLimitExceeded,   // TLE
    MemoryLimitExceeded, // MLE
    OutputLimitExceeded, // OLE
    RuntimeError,        // RE
    RestrictedFunction,  // RF
    CompileError,        // CE
    JudgeFailed,         // SE
    Skipped,             // SKIPPED
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TleReason {
    Cpu,
    Wall,
}

impl Verdict {
    /// Return the canonical 2-letter / short uppercase code.
    pub fn short_code(self) -> &'static str {
        match self {
            Self::Accepted => "AC",
            Self::WrongAnswer => "WA",
            Self::PresentationError => "PE",
            Self::PartiallyCorrect => "PC",
            Self::TimeLimitExceeded => "TLE",
            Self::MemoryLimitExceeded => "MLE",
            Self::OutputLimitExceeded => "OLE",
            Self::RuntimeError => "RE",
            Self::RestrictedFunction => "RF",
            Self::CompileError => "CE",
            Self::JudgeFailed => "SE",
            Self::Skipped => "SKIPPED",
        }
    }

    /// Full human-readable display name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Accepted => "Accepted",
            Self::WrongAnswer => "Wrong Answer",
            Self::PresentationError => "Presentation Error",
            Self::PartiallyCorrect => "Partially Correct",
            Self::TimeLimitExceeded => "Time Limit Exceeded",
            Self::MemoryLimitExceeded => "Memory Limit Exceeded",
            Self::OutputLimitExceeded => "Output Limit Exceeded",
            Self::RuntimeError => "Runtime Error",
            Self::RestrictedFunction => "Restricted Function",
            Self::CompileError => "Compile Error",
            Self::JudgeFailed => "Judge Failed",
            Self::Skipped => "Skipped",
        }
    }

    pub fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }

    /// Priority order for determining overall verdict of a run.
    /// Higher number = higher precedence.
    /// SE > CE > RF > MLE > OLE > TLE > RE > WA > PE > PC > AC > Skipped
    pub fn priority(self) -> u8 {
        match self {
            Self::JudgeFailed => 110,
            Self::CompileError => 100,
            Self::RestrictedFunction => 90,
            Self::MemoryLimitExceeded => 80,
            Self::OutputLimitExceeded => 70,
            Self::TimeLimitExceeded => 60,
            Self::RuntimeError => 50,
            Self::WrongAnswer => 40,
            Self::PresentationError => 30,
            Self::PartiallyCorrect => 20,
            Self::Accepted => 10,
            Self::Skipped => 0,
        }
    }

    /// Merge two verdicts, keeping the one with higher priority.
    pub fn merge(self, other: Self) -> Self {
        if self.priority() >= other.priority() {
            self
        } else {
            other
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short_code())
    }
}

impl FromStr for Verdict {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_uppercase();
        match normalized.as_str() {
            "AC" | "ACCEPTED" => Ok(Self::Accepted),
            "WA" | "WRONG_ANSWER" | "WRONG ANSWER" => Ok(Self::WrongAnswer),
            "PE" | "PRESENTATION_ERROR" | "PRESENTATION ERROR" => Ok(Self::PresentationError),
            "PC" | "PARTIALLY_CORRECT" | "PARTIALLY CORRECT" | "PARTIAL" => {
                Ok(Self::PartiallyCorrect)
            }
            "TLE" | "TIME_LIMIT_EXCEEDED" | "TIME_LIMIT" | "TIME LIMIT EXCEEDED" => {
                Ok(Self::TimeLimitExceeded)
            }
            "MLE" | "MEMORY_LIMIT_EXCEEDED" | "MEMORY_LIMIT" | "MEMORY LIMIT EXCEEDED" => {
                Ok(Self::MemoryLimitExceeded)
            }
            "OLE" | "OUTPUT_LIMIT_EXCEEDED" | "OUTPUT_LIMIT" | "OUTPUT LIMIT EXCEEDED" => {
                Ok(Self::OutputLimitExceeded)
            }
            "RE" | "RUNTIME_ERROR" | "RUNTIME ERROR" => Ok(Self::RuntimeError),
            "RF" | "RESTRICTED_FUNCTION" | "RESTRICTED FUNCTION" => Ok(Self::RestrictedFunction),
            "CE" | "COMPILE_ERROR" | "COMPILE ERROR" => Ok(Self::CompileError),
            "SE" | "JUDGE_FAILED" | "JUDGE FAILED" | "SYSTEM_ERROR" | "FAIL" => {
                Ok(Self::JudgeFailed)
            }
            "SKIPPED" | "SKIP" => Ok(Self::Skipped),
            _ => Err(format!("unknown verdict: '{s}'")),
        }
    }
}

impl Serialize for Verdict {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.short_code())
    }
}

impl<'de> Deserialize<'de> for Verdict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verdict_serialization() {
        assert_eq!(serde_json::to_string(&Verdict::Accepted).unwrap(), "\"AC\"");
        assert_eq!(
            serde_json::to_string(&Verdict::WrongAnswer).unwrap(),
            "\"WA\""
        );
        assert_eq!(
            serde_json::to_string(&Verdict::JudgeFailed).unwrap(),
            "\"SE\""
        );

        let v: Verdict = serde_json::from_str("\"AC\"").unwrap();
        assert_eq!(v, Verdict::Accepted);
        let v: Verdict = serde_json::from_str("\"WA\"").unwrap();
        assert_eq!(v, Verdict::WrongAnswer);
        let v: Verdict = serde_json::from_str("\"tle\"").unwrap();
        assert_eq!(v, Verdict::TimeLimitExceeded);
    }

    #[test]
    fn test_verdict_priority() {
        assert!(Verdict::JudgeFailed.priority() > Verdict::CompileError.priority());
        assert!(Verdict::WrongAnswer.priority() > Verdict::Accepted.priority());
        assert!(Verdict::TimeLimitExceeded.priority() > Verdict::Accepted.priority());
    }
}
