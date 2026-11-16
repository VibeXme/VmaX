/// Cloud invitations use a separate path from existing LAN/relay invites.
pub fn link(device: &str, token: &str) -> String {
    format!("vmax://join/cloud/{device}/{token}")
}
pub fn parse(link: &str) -> Option<(&str, &str)> {
    let path = link.strip_prefix("vmax://join/cloud/")?;
    let (device, token) = path.split_once('/')?;
    (device.len() == 36 && token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some((device, token))
}

#[cfg(test)]
#[test]
fn accepts_only_complete_cloud_links() {
    let device = "00000000-0000-4000-8000-000000000000";
    let token = "a".repeat(64);
    let url = link(device, &token);
    assert_eq!(parse(&url), Some((device, token.as_str())));
    assert!(parse("vmax://join/ABCD-EFGH").is_none());
    assert!(parse(&format!("{url}?token=bogus")).is_none());
}
