use tokio::net::TcpListener;

use super::listener::serve_pg_listener;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_extended_query_handles_params_and_typed_rows() {
    let cfg = crate::config::DbConfig {
        read_workers: 1,
        write_queue_size: 8,
        read_queue_size: 8,
        snapshot_queue_size: 1,
        max_result_rows: 100,
        ..crate::config::DbConfig::default()
    };
    let db = crate::db::DbHandle::open(None, &cfg);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_db = db.clone();
    let server = tokio::spawn(async move {
        serve_pg_listener(listener, server_db).await;
    });

    let connect_string = format!(
        "host=127.0.0.1 port={} user=admin password=admin dbname=memory",
        addr.port()
    );
    let (client, connection) = tokio_postgres::connect(&connect_string, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection_task = tokio::spawn(async move {
        let _ = connection.await;
    });

    let ok = "1";
    let rows = client
        .query("select $1::varchar as ok", &[&ok])
        .await
        .unwrap();
    let text_value: String = rows[0].try_get(0).unwrap();
    assert_eq!(text_value, "1");

    let rows = client.query("select 1::integer as ok", &[]).await.unwrap();
    let int_value: i32 = rows[0].try_get(0).unwrap();
    assert_eq!(int_value, 1);

    let param_value = 2_i32;
    let rows = client
        .query("select $1::integer as ok", &[&param_value])
        .await
        .unwrap();
    let int_param_value: i32 = rows[0].try_get(0).unwrap();
    assert_eq!(int_param_value, 2);

    let rows = client
        .query("select 12.34::decimal(10, 2) as amount", &[])
        .await
        .unwrap();
    let decimal_value: rust_decimal::Decimal = rows[0].try_get(0).unwrap();
    assert_eq!(decimal_value.to_string(), "12.34");

    let uuid_text = "550e8400-e29b-41d4-a716-446655440000";
    let rows = client
        .query(
            "select cast('550e8400-e29b-41d4-a716-446655440000' as uuid) as id",
            &[],
        )
        .await
        .unwrap();
    let uuid_value: uuid::Uuid = rows[0].try_get(0).unwrap();
    assert_eq!(uuid_value.to_string(), uuid_text);

    connection_task.abort();
    server.abort();
    db.shutdown();
}
