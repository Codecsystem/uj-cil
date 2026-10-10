//! Built-in checkers (exact, wcmp, fcmp, lcmp, ncmp, hcmp, rcmp4/6/9, yesno).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCheckerType {
    Exact,
    Wcmp,
    Fcmp,
    Lcmp,
    Ncmp,
    Hcmp,
    Rcmp4,
    Rcmp6,
    Rcmp9,
    Yesno,
}
