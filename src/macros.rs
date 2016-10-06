macro_rules! task_log {
    ($lvl:expr, $($args:tt)+) => {{
        use CONTEXT;

        let o = CONTEXT.with(move |m| {
            m.borrow().as_ref().map(|c| c.logger.clone())
        });
        if let Some(log) = o {
            log!($lvl, log, $($args)+)
        } else {
            log!($lvl, ::DEFAULT_LOGGER, $($args)+)
        }
    }}
}

macro_rules! task_trace {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Trace, $($args)+);
    }}
}

macro_rules! task_debug {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Debug, $($args)+);
    }}
}

macro_rules! task_info {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Info, $($args)+);
    }}
}

macro_rules! task_warn {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Warning, $($args)+);
    }}
}

macro_rules! task_error {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Error, $($args)+);
    }}
}

macro_rules! task_crit {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Crit, $($args)+);
    }}
}
