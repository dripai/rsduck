use pgwire::api::{ClientInfo, METADATA_DATABASE, METADATA_USER};

pub(super) fn metadata_value<'a, C>(client: &'a C, key: &str) -> &'a str
where
    C: ClientInfo,
{
    client
        .metadata()
        .get(key)
        .map(|value| value.as_str())
        .unwrap_or("")
}

pub(super) fn session_user<C>(client: &C) -> &str
where
    C: ClientInfo,
{
    client
        .metadata()
        .get(METADATA_USER)
        .map(|value| value.as_str())
        .unwrap_or("admin")
}

pub(super) fn session_database<C>(client: &C) -> &str
where
    C: ClientInfo,
{
    metadata_value(client, METADATA_DATABASE)
}
