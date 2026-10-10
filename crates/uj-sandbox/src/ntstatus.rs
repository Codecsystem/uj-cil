//! NTSTATUS code mappings for Windows execution.

pub fn format_ntstatus(code: u32) -> &'static str {
    match code {
        0x00000000 => "STATUS_SUCCESS",
        0xC0000005 => "STATUS_ACCESS_VIOLATION",
        0xC00000FD => "STATUS_STACK_OVERFLOW",
        0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
        0xC0000409 => "STATUS_STACK_BUFFER_OVERRUN",
        0xC0000017 => "STATUS_NO_MEMORY",
        0xC000001D => "STATUS_ILLEGAL_INSTRUCTION",
        0xC0000135 => "STATUS_DLL_NOT_FOUND",
        0xC0000096 => "STATUS_PRIVILEGED_INSTRUCTION",
        0xC0000025 => "STATUS_NONCONTINUABLE_EXCEPTION",
        0xC0000026 => "STATUS_INVALID_DISPOSITION",
        0xC000008C => "STATUS_ARRAY_BOUNDS_EXCEEDED",
        0xC000008D => "STATUS_FLOAT_DENORMAL_OPERAND",
        0xC000008E => "STATUS_FLOAT_DIVIDE_BY_ZERO",
        0xC000008F => "STATUS_FLOAT_INEXACT_RESULT",
        0xC0000090 => "STATUS_FLOAT_INVALID_OPERATION",
        0xC0000091 => "STATUS_FLOAT_OVERFLOW",
        0xC0000092 => "STATUS_FLOAT_STACK_CHECK",
        0xC0000093 => "STATUS_FLOAT_UNDERFLOW",
        0xC0000095 => "STATUS_INTEGER_OVERFLOW",
        0x80000003 => "STATUS_BREAKPOINT",
        0x80000004 => "STATUS_SINGLE_STEP",
        0xC0000043 => "STATUS_SHARING_VIOLATION",
        0xC0000034 => "STATUS_OBJECT_NAME_NOT_FOUND",
        0xC0000022 => "STATUS_ACCESS_DENIED",
        _ => "STATUS_UNRECOGNIZED_EXCEPTION",
    }
}

pub fn is_ntstatus_exception(code: u32) -> bool {
    // NTSTATUS error severity: top two bits == 11b (0xC0000000..) or warning/info
    (code & 0xC0000000) == 0xC0000000 || (code & 0x80000000) == 0x80000000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ntstatus_mapping() {
        assert_eq!(format_ntstatus(0xC0000005), "STATUS_ACCESS_VIOLATION");
        assert_eq!(format_ntstatus(0xC00000FD), "STATUS_STACK_OVERFLOW");
        assert_eq!(format_ntstatus(0xC0000094), "STATUS_INTEGER_DIVIDE_BY_ZERO");
        assert_eq!(format_ntstatus(0xC0000135), "STATUS_DLL_NOT_FOUND");
        assert!(is_ntstatus_exception(0xC0000005));
        assert!(!is_ntstatus_exception(0));
    }
}
