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

#![allow(unused_macros)]

// $ctx in all macro arguments is of type crate::ConnectionContext

macro_rules! task_log {
    ($ctx:expr, $lvl:expr, $($args:tt)+) => {{
        log!($ctx.logger.as_ref(), $lvl, "", $($args)+)
    }}
}

macro_rules! task_trace {
    ($ctx:expr, $($args:tt)+) => {{
        task_log!($ctx, ::slog::Level::Trace, $($args)+);
    }}
}

macro_rules! task_debug {
    ($ctx:expr, $($args:tt)+) => {{
        task_log!($ctx, ::slog::Level::Debug, $($args)+);
    }}
}

macro_rules! task_info {
    ($ctx:expr, $($args:tt)+) => {{
        task_log!($ctx, ::slog::Level::Info, $($args)+);
    }}
}

macro_rules! task_warn {
    ($ctx:expr, $($args:tt)+) => {{
        task_log!($ctx, ::slog::Level::Warning, $($args)+);
    }};
}

macro_rules! task_error {
    ($ctx:expr, $($args:tt)+) => {{
        task_log!($ctx, ::slog::Level::Error, $($args)+);
    }}
}

macro_rules! task_crit {
    ($ctx:expr, $($args:tt)+) => {{
        task_log!($ctx, ::slog::Level::Crit, $($args)+);
    }}
}
