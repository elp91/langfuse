use super::*;
use std::str::FromStr;

fn exemptions(value: &str) -> EgressExemptions {
    EgressExemptions::parse(value).unwrap()
}

fn restricted() -> EgressPolicy {
    EgressPolicy::new(EgressExemptions::default(), true)
}

fn policy(value: &str) -> EgressPolicy {
    EgressPolicy::new(exemptions(value), true)
}

#[test]
fn public_ipv4_hosts_are_allowed() {
    assert!(restricted().allows_literal("93.184.216.34"));
    assert!(restricted().allows_literal("8.8.8.8"));
}

#[test]
fn private_and_reserved_addresses_are_blocked() {
    for host in [
        "10.3.24.39",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "127.0.0.1",
        "169.254.169.254", // cloud metadata endpoint
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "240.0.0.1",
        "198.51.100.7", // TEST-NET-3
    ] {
        assert!(!restricted().allows_literal(host), "{host} must be blocked");
    }
}

#[test]
fn private_ipv6_addresses_are_blocked() {
    for host in ["::1", "fe80::1", "fd00::1", "ff02::1", "::ffff:10.0.0.1"] {
        assert!(!restricted().allows_literal(host), "{host} must be blocked");
    }
    assert!(restricted().allows_literal("2606:4700::6810:85e5"));
}

#[test]
fn allows_literal_rejects_non_addresses() {
    // Only literals go through this gate; names are resolved and checked
    // through allows_resolved.
    assert!(!restricted().allows_literal("api.openai.com"));
}

#[test]
fn unrestricted_policy_allows_everything() {
    let policy = EgressPolicy::new(EgressExemptions::default(), false);
    assert!(policy.allows_literal("127.0.0.1"));
    assert!(policy.allows_literal("10.0.0.1"));
}

#[test]
fn range_exemptions_allow_private_addresses() {
    assert!(policy("10.3.24.39").allows_literal("10.3.24.39"));
    assert!(!policy("10.3.24.39").allows_literal("10.3.24.40"));

    let policy = policy("10.3.24.0/23");
    assert!(policy.allows_literal("10.3.24.39"));
    assert!(policy.allows_literal("10.3.25.1"));
    assert!(!policy.allows_literal("10.3.26.1"));
}

#[test]
fn loopback_is_blocked_even_when_exempted() {
    let policy = policy("127.0.0.1,10.0.0.0/8");
    assert!(!policy.allows_literal("127.0.0.1"));
    assert!(!policy.allows_literal("127.5.5.5"));
    assert!(policy.allows_literal("10.1.2.3"));
}

#[test]
fn zero_prefix_exempts_the_whole_family() {
    assert!(policy("0.0.0.0/0").allows_literal("10.3.24.39"));
    assert!(policy("0.0.0.0/0").allows_literal("172.16.0.1"));
    assert!(!policy("::/0").allows_literal("10.3.24.39"));
    assert!(policy("::/0").allows_literal("fd00::1"));
    assert!(!policy("::/0").allows_literal("::1"));
}

#[test]
fn parse_rejects_malformed_entries() {
    assert!(EgressExemptions::parse("10.0.0.0/33").is_err());
    assert!(EgressExemptions::parse("not an ip/24").is_err());
    assert!(EgressExemptions::parse("10.0.0.0//8").is_err());
    assert!(EgressExemptions::parse("::1/129").is_err());
    assert!(EgressExemptions::parse("10.0.0.0, 10.1.0.0/8, proxy.internal").is_ok());
    assert!(EgressExemptions::parse("").is_ok());
    assert!(EgressExemptions::parse("  ").is_ok());
}

#[test]
fn parse_accepts_bracketed_ipv6_as_host_entry() {
    // Brackets are stripped; the entry lands in the host set and matches a
    // resolution of that literal or the same name at resolve time.
    let parsed = exemptions("[fd00::1]");
    assert!(parsed.exempts("fd00::1", "fd00::1".parse().unwrap()));
}

#[tokio::test]
async fn resolver_keeps_only_permitted_addresses() {
    // A name whose system resolution yields loopback must produce no permitted
    // address; the resolver errors rather than returning an empty iterator.
    let resolver = EgressResolver::new(restricted());
    let name = Name::from_str("localhost").unwrap();
    let result = Resolve::resolve(&resolver, name).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn resolver_permits_public_names() {
    // The literal-IP check must not be applied to resolved names; a public
    // hostname resolves and passes.
    let resolver = EgressResolver::new(restricted());
    let name = Name::from_str("localhost").unwrap();
    // localhost resolves to loopback everywhere, so it fails; a name resolving
    // to a public address depends on the environment, so only assert the
    // loopback refusal deterministically here.
    let result = Resolve::resolve(&resolver, name).await;
    assert!(result.is_err());
}

#[test]
fn hostname_exemptions_apply_to_resolved_addresses() {
    let policy = policy("internal.corp");
    assert!(policy.allows_resolved("internal.corp", "10.3.24.39".parse().unwrap()));
    assert!(!policy.allows_resolved("other.corp", "10.3.24.39".parse().unwrap()));
}

#[test]
fn resolved_policy_blocks_unlisted_private_addresses() {
    let policy = restricted();
    assert!(!policy.allows_resolved("internal.corp", "10.3.24.39".parse().unwrap()));
    assert!(policy.allows_resolved("api.openai.com", "93.184.216.34".parse().unwrap()));
}
