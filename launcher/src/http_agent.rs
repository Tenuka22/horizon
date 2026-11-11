//! The agent the launcher's downloads go through: its own user agent, the player's proxy, and one
//! agent per origin so connections are kept and reused.

use std::sync::Mutex;

type Agents = std::collections::HashMap<String, ureq::Agent>;
static AGENTS: Mutex<Option<Agents>> = Mutex::new(None);

/// Forgets the pooled agents, so the next request builds its own with the proxy as it is now.
pub(crate) fn forget_agents() {
    if let Ok(mut agents) = AGENTS.lock() {
        *agents = None;
    }
}

/// The agent `url` is fetched with: the launcher's own user agent, through the player's proxy when
/// one is set (`proxy`). One agent serves every request to the same origin, so its connections
/// are kept and reused: a download of thousands of small files (Minecraft's sounds) would
/// otherwise open, and set up TLS for, a connection per file.
pub(crate) fn agent(url: &str) -> ureq::Agent {
    let origin = url
        .split_once("://")
        .map(|(scheme, rest)| {
            format!(
                "{scheme}://{}",
                rest.split(['/', '?', '#']).next().unwrap_or_default()
            )
        })
        .unwrap_or_else(|| url.to_owned());
    let Ok(mut agents) = AGENTS.lock() else {
        return new_agent(url);
    };
    agents
        .get_or_insert_with(Agents::new)
        .entry(origin)
        .or_insert_with(|| new_agent(url))
        .clone()
}

fn new_agent(url: &str) -> ureq::Agent {
    crate::proxy::agent(
        ureq::AgentBuilder::new()
            .user_agent(&format!("horizon-launcher/{}", crate::backend::VERSION))
            // A connection that goes quiet fails instead of waiting for ever.
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            // Every download worker keeps its connection for the next file.
            .max_idle_connections_per_host(32),
        url,
    )
    .build()
}
