//! Present the tunnel's lifecycle separately from per-client forwarding errors.
use super::*;

pub(crate) fn status(tunnel: &TunnelState) -> String {
    if let Some(retry) = tunnel.retry {
        return match retry.delay_secs {
            Some(delay) => tf(
                "隧道已断开，{} 秒后重拨（{}/{}）",
                &[&delay, &retry.attempt, &retry.max_attempts],
            ),
            None => tf(
                "隧道正在重拨（{}/{}）",
                &[&retry.attempt, &retry.max_attempts],
            ),
        };
    }
    if tunnel.stopped {
        if let Some(attempts) = tunnel.retries_exhausted {
            return tf(
                "隧道重拨已耗尽（{} 次）：{}",
                &[
                    &attempts,
                    &runtime(tunnel.error.as_deref().unwrap_or_default()),
                ],
            );
        }
        return tunnel.error.as_ref().map_or_else(
            || t("隧道已停止").to_owned(),
            |error| tf("隧道已停止：{}", &[&runtime(error)]),
        );
    }
    if tunnel.listening {
        return tunnel.error.as_ref().map_or_else(
            || tf("隧道正在监听 {}", &[&tunnel.bind]),
            |error| tf("监听中；最近错误：{}", &[&runtime(error)]),
        );
    }
    t("隧道正在连接…").to_owned()
}
