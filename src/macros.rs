// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

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
