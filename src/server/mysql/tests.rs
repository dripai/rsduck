use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};

use super::codec::{put_lenenc_bytes, put_lenenc_int, put_null_str, put_u32_le};
use super::codec::{read_packet, write_packet};
use super::handshake::parse_handshake_response;
use super::listener::serve_mysql_listener;
use super::types::{
    CLIENT_CONNECT_ATTRS, CLIENT_CONNECT_WITH_DB, CLIENT_LONG_PASSWORD, CLIENT_PLUGIN_AUTH,
    CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
    MYSQL_TYPE_JSON, MYSQL_TYPE_LONG,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_protocol_handles_auth_query_and_prepared_execute() {
    let cfg = crate::config::DbConfig {
        read_workers: 1,
        write_queue_size: 8,
        read_queue_size: 8,
        snapshot_queue_size: 1,
        max_result_rows: 100,
        vss_enabled: false,
        ..crate::config::DbConfig::default()
    };
    let db = crate::db::DbHandle::open(None, &cfg);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_db = db.clone();
    let server = tokio::spawn(async move {
        serve_mysql_listener(listener, server_db).await;
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let handshake = read_packet(&mut stream).await.unwrap();
    let nonce = nonce_from_handshake(&handshake.payload);
    let auth_payload = client_handshake_response("admin", "admin", &nonce);
    let mut seq = 1_u8;
    write_packet(&mut stream, &mut seq, &auth_payload)
        .await
        .unwrap();
    let auth_more = read_packet(&mut stream).await.unwrap();
    assert_eq!(auth_more.payload, vec![0x01, 0x03]);
    let ok = read_packet(&mut stream).await.unwrap();
    assert_eq!(ok.payload.first(), Some(&0x00));

    let mut query = vec![0x03];
    query.extend_from_slice(b"select 1::integer as ok");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let column_count = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_count.payload, vec![1]);
    let _column = read_packet(&mut stream).await.unwrap();
    let _eof = read_packet(&mut stream).await.unwrap();
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(row.payload, vec![1, b'1']);
    let _eof = read_packet(&mut stream).await.unwrap();

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW VARIABLES LIKE 'lower_case_%'");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let column_count = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_count.payload, vec![2]);
    let _variable_name = read_packet(&mut stream).await.unwrap();
    let _value = read_packet(&mut stream).await.unwrap();
    let _eof = read_packet(&mut stream).await.unwrap();
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(
        text_cells(&row.payload),
        vec!["lower_case_file_system", "ON"]
    );
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(
        text_cells(&row.payload),
        vec!["lower_case_table_names", "1"]
    );
    let _eof = read_packet(&mut stream).await.unwrap();

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT * FROM information_schema.ENGINES");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let column_count = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_count.payload, vec![6]);
    for _ in 0..6 {
        let _column = read_packet(&mut stream).await.unwrap();
    }
    let _eof = read_packet(&mut stream).await.unwrap();
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(
        text_cells(&row.payload),
        vec![
            "InnoDB",
            "DEFAULT",
            "rsduck MySQL protocol compatibility engine",
            "YES",
            "NO",
            "YES",
        ]
    );
    let _eof = read_packet(&mut stream).await.unwrap();

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 3).await;
    assert!(rows.contains(&vec![
        "main".to_string(),
        "UTF8".to_string(),
        "utf8mb4_general_ci".to_string(),
    ]));

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW PROCEDURE STATUS WHERE Db = 'main'");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 11).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"SELECT user, host, ssl_type, ssl_cipher, x509_issuer, x509_subject, max_questions, max_updates, max_connections, super_priv, max_user_connections, plugin, password_expired, password_lifetime FROM mysql.user ORDER BY user",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 14).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "admin");
    assert_eq!(rows[0][1], "%");
    assert_eq!(rows[0][9], "Y");
    assert_eq!(rows[0][11], "caching_sha2_password");
    assert_eq!(rows[0][12], "N");

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT * FROM mysql.user u");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 51).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "%");
    assert_eq!(rows[0][1], "admin");

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT FROM_HOST, FROM_USER, TO_HOST, TO_USER FROM mysql.role_edges");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 4).await;
    assert!(rows.contains(&vec![
        "%".to_string(),
        "admin".to_string(),
        "%".to_string(),
        "admin".to_string(),
    ]));

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"SELECT DEFAULT_ROLE_HOST, DEFAULT_ROLE_USER, HOST, USER FROM mysql.default_roles",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 4).await;
    assert!(rows.contains(&vec![
        "%".to_string(),
        "admin".to_string(),
        "%".to_string(),
        "admin".to_string(),
    ]));

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT db.Host, db.Db, db.User, db.Select_priv, db.Insert_priv, db.Update_priv, db.Delete_priv, db.Create_priv, db.Drop_priv, db.Grant_priv, db.References_priv, db.Index_priv, db.Alter_priv, db.Create_tmp_table_priv, db.Lock_tables_priv, db.Create_view_priv, db.Show_view_priv, db.Create_routine_priv, db.Alter_routine_priv, db.Execute_priv, db.Event_priv, db.Trigger_priv FROM mysql.db db");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 22).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT pp.Host, pp.Db, pp.User, pp.Routine_name, pp.Routine_type, pp.Proc_priv FROM mysql.procs_priv pp ORDER BY pp.User");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 6).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT tp.Host, tp.Db, tp.User, tp.Table_name, tp.Table_priv FROM mysql.tables_priv tp WHERE tp.Table_priv != '' ORDER BY tp.User");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 5).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT cp.Host, cp.Db, cp.User, cp.Table_name, cp.Column_name, cp.Column_priv FROM mysql.columns_priv cp ORDER BY cp.User");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 6).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW FUNCTION STATUS WHERE Db = 'main'");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 11).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(b"CREATE TABLE mysql_quotes(code VARCHAR, price DOUBLE)");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let ok = read_packet(&mut stream).await.unwrap();
    assert_eq!(ok.payload.first(), Some(&0x00));

    let mut query = vec![0x03];
    query.extend_from_slice(b"INSERT INTO mysql_quotes VALUES ('AAPL', 1.5), ('MSFT', 2.5)");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let ok = read_packet(&mut stream).await.unwrap();
    assert_eq!(ok.payload.first(), Some(&0x00));

    let mut query = vec![0x03];
    query.extend_from_slice(b"CREATE VIEW mysql_quote_view AS SELECT code FROM mysql_quotes");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let ok = read_packet(&mut stream).await.unwrap();
    assert_eq!(ok.payload.first(), Some(&0x00));

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"SELECT table_schema, table_name, view_definition FROM information_schema.views WHERE table_name = 'mysql_quote_view'",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 3).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "main");
    assert_eq!(rows[0][1], "mysql_quote_view");
    assert_eq!(
        rows[0][2],
        "CREATE VIEW mysql_quote_view AS SELECT code FROM mysql_quotes;"
    );

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"SELECT routine_schema, routine_name, routine_type FROM information_schema.routines",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 3).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"SELECT specific_schema, specific_name, parameter_name FROM information_schema.parameters",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 3).await;
    assert!(rows.is_empty());

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW FULL TABLES WHERE Table_type != 'VIEW'");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 2).await;
    assert!(rows.contains(&vec!["mysql_quotes".to_string(), "BASE TABLE".to_string(),]));
    assert!(!rows.iter().any(|row| row[0] == "mysql_quote_view"));

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW TABLE STATUS");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 18).await;
    let table_status = rows
        .iter()
        .find(|row| row[0] == "mysql_quotes")
        .expect("created table status row");
    assert_eq!(table_status[1], "InnoDB");
    assert_eq!(table_status[3], "Dynamic");
    assert_eq!(table_status[14], "utf8mb4_general_ci");
    let view_status = rows
        .iter()
        .find(|row| row[0] == "mysql_quote_view")
        .expect("created view status row");
    assert_eq!(view_status[1], "NULL");
    assert_eq!(view_status[17], "VIEW");

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW COLUMNS FROM mysql_quotes");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 6).await;
    assert_eq!(
        rows,
        vec![
            vec![
                "code".to_string(),
                "varchar".to_string(),
                "YES".to_string(),
                "".to_string(),
                "NULL".to_string(),
                "".to_string(),
            ],
            vec![
                "price".to_string(),
                "double".to_string(),
                "YES".to_string(),
                "".to_string(),
                "NULL".to_string(),
                "".to_string(),
            ],
        ]
    );

    let mut query = vec![0x03];
    query.extend_from_slice(b"DESCRIBE mysql_quotes");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 6).await;
    assert_eq!(rows[0][0], "code");
    assert_eq!(rows[1][0], "price");

    let mut query = vec![0x03];
    query.extend_from_slice(b"CREATE INDEX idx_mysql_quotes_code ON mysql_quotes(code)");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let ok = read_packet(&mut stream).await.unwrap();
    assert_eq!(ok.payload.first(), Some(&0x00));

    let mut query = vec![0x03];
    query.extend_from_slice(b"SHOW INDEX FROM mysql_quotes");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 15).await;
    assert!(rows.iter().any(|row| {
        row[0] == "mysql_quotes"
            && row[2] == "idx_mysql_quotes_code"
            && row[4] == "code"
            && row[10] == "BTREE"
    }));

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT code FROM `main`.`mysql_quotes` ORDER BY code LIMIT 1");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 1).await;
    assert_eq!(rows, vec![vec!["AAPL".to_string()]]);

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT code, price FROM mysql_quotes ORDER BY code LIMIT 0, 1");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let rows = read_text_resultset(&mut stream, 2).await;
    assert_eq!(rows, vec![vec!["AAPL".to_string(), "1.5".to_string()]]);

    let mut query = vec![0x03];
    query.extend_from_slice(
        b"select [1, NULL, 2] as items, {'code': 'AAPL', 'price': 1.5, 'halted': NULL} as quote, map(['a', 'b'], [1, NULL]) as labels, array_value(1, NULL, 3) as fixed_items",
    );
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let column_count = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_count.payload, vec![4]);
    for _ in 0..4 {
        let column = read_packet(&mut stream).await.unwrap();
        assert_eq!(column_type(&column.payload), MYSQL_TYPE_JSON);
    }
    let _eof = read_packet(&mut stream).await.unwrap();
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(
        text_cells(&row.payload),
        vec![
            "[1,null,2]",
            r#"{"code":"AAPL","halted":null,"price":"1.5"}"#,
            r#"[{"key":"a","value":1},{"key":"b","value":null}]"#,
            "[1,null,3]",
        ]
    );
    let _eof = read_packet(&mut stream).await.unwrap();

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT [0.125, 0.25, 0.5]::FLOAT[3] AS embedding");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &query).await.unwrap();
    let column_count = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_count.payload, vec![1]);
    let column = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_type(&column.payload), MYSQL_TYPE_JSON);
    let _eof = read_packet(&mut stream).await.unwrap();
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(text_cells(&row.payload), vec!["[0.125,0.25,0.5]"]);
    let _eof = read_packet(&mut stream).await.unwrap();

    let mut prepare = vec![0x16];
    prepare.extend_from_slice(b"select ?::integer as ok");
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &prepare).await.unwrap();
    let prepare_ok = read_packet(&mut stream).await.unwrap();
    assert_eq!(prepare_ok.payload.first(), Some(&0x00));
    let statement_id = u32::from_le_bytes([
        prepare_ok.payload[1],
        prepare_ok.payload[2],
        prepare_ok.payload[3],
        prepare_ok.payload[4],
    ]);
    let param_count = u16::from_le_bytes([prepare_ok.payload[7], prepare_ok.payload[8]]);
    let column_count = u16::from_le_bytes([prepare_ok.payload[5], prepare_ok.payload[6]]);
    assert_eq!(param_count, 1);
    assert_eq!(column_count, 1);
    let _param = read_packet(&mut stream).await.unwrap();
    let _eof = read_packet(&mut stream).await.unwrap();
    let _column = read_packet(&mut stream).await.unwrap();
    let _eof = read_packet(&mut stream).await.unwrap();

    let mut execute = vec![0x17];
    put_u32_le(&mut execute, statement_id);
    execute.push(0);
    put_u32_le(&mut execute, 1);
    execute.push(0);
    execute.push(1);
    execute.push(MYSQL_TYPE_LONG);
    execute.push(0);
    execute.extend_from_slice(&42_i32.to_le_bytes());
    let mut seq = 0_u8;
    write_packet(&mut stream, &mut seq, &execute).await.unwrap();
    let column_count = read_packet(&mut stream).await.unwrap();
    assert_eq!(column_count.payload, vec![1]);
    let _column = read_packet(&mut stream).await.unwrap();
    let _eof = read_packet(&mut stream).await.unwrap();
    let row = read_packet(&mut stream).await.unwrap();
    assert_eq!(&row.payload[row.payload.len() - 4..], &42_i32.to_le_bytes());

    server.abort();
    db.shutdown();
}

async fn read_text_resultset(stream: &mut TcpStream, expected_columns: usize) -> Vec<Vec<String>> {
    let column_count = read_packet(stream).await.unwrap();
    assert_eq!(column_count.payload, vec![expected_columns as u8]);
    for _ in 0..expected_columns {
        let _column = read_packet(stream).await.unwrap();
    }
    let eof = read_packet(stream).await.unwrap();
    assert!(is_eof_packet(&eof.payload));

    let mut rows = Vec::new();
    loop {
        let packet = read_packet(stream).await.unwrap();
        if is_eof_packet(&packet.payload) {
            break;
        }
        rows.push(text_cells(&packet.payload));
    }
    rows
}

fn is_eof_packet(payload: &[u8]) -> bool {
    payload.first() == Some(&0xfe) && payload.len() < 9
}

fn column_type(payload: &[u8]) -> u8 {
    let mut idx = 0;
    for _ in 0..6 {
        let len = read_lenenc_int(payload, &mut idx) as usize;
        idx += len;
    }
    assert_eq!(payload[idx], 0x0c);
    idx += 1 + 2 + 4;
    payload[idx]
}

fn text_cells(payload: &[u8]) -> Vec<String> {
    let mut idx = 0;
    let mut cells = Vec::new();
    while idx < payload.len() {
        if payload[idx] == 0xfb {
            cells.push("NULL".to_string());
            idx += 1;
            continue;
        }
        let len = read_lenenc_int(payload, &mut idx) as usize;
        cells.push(String::from_utf8(payload[idx..idx + len].to_vec()).unwrap());
        idx += len;
    }
    cells
}

fn read_lenenc_int(payload: &[u8], idx: &mut usize) -> u64 {
    let first = payload[*idx];
    *idx += 1;
    match first {
        0xfc => {
            let value = u16::from_le_bytes([payload[*idx], payload[*idx + 1]]);
            *idx += 2;
            value as u64
        }
        0xfd => {
            let value =
                u32::from_le_bytes([payload[*idx], payload[*idx + 1], payload[*idx + 2], 0]);
            *idx += 3;
            value as u64
        }
        0xfe => {
            let value = u64::from_le_bytes([
                payload[*idx],
                payload[*idx + 1],
                payload[*idx + 2],
                payload[*idx + 3],
                payload[*idx + 4],
                payload[*idx + 5],
                payload[*idx + 6],
                payload[*idx + 7],
            ]);
            *idx += 8;
            value
        }
        value => value as u64,
    }
}

fn nonce_from_handshake(payload: &[u8]) -> Vec<u8> {
    let mut idx = 1;
    while payload[idx] != 0 {
        idx += 1;
    }
    idx += 1;
    idx += 4;
    let mut nonce = payload[idx..idx + 8].to_vec();
    idx += 9;
    idx += 2 + 1 + 2 + 2;
    let auth_len = payload[idx] as usize;
    idx += 1 + 10;
    let second_len = auth_len.saturating_sub(8).max(12);
    nonce.extend_from_slice(&payload[idx..idx + second_len - 1]);
    nonce.truncate(20);
    nonce
}

fn client_handshake_response(username: &str, password: &str, nonce: &[u8]) -> Vec<u8> {
    let capabilities = CLIENT_LONG_PASSWORD
        | CLIENT_PROTOCOL_41
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH
        | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
        | CLIENT_CONNECT_ATTRS
        | CLIENT_CONNECT_WITH_DB;
    let mut out = Vec::new();
    put_u32_le(&mut out, capabilities);
    put_u32_le(&mut out, 16 * 1024 * 1024);
    out.push(45);
    out.extend_from_slice(&[0_u8; 23]);
    put_null_str(&mut out, username);
    put_lenenc_bytes(&mut out, &caching_sha2_token(password, nonce));
    put_null_str(&mut out, "main");
    put_null_str(&mut out, crate::auth::MYSQL_DEFAULT_AUTH_PLUGIN);
    let mut attrs = Vec::new();
    put_lenenc_bytes(&mut attrs, b"_client_name");
    put_lenenc_bytes(&mut attrs, b"rsduck-test");
    put_lenenc_int(&mut out, attrs.len() as u64);
    out.extend_from_slice(&attrs);
    let parsed = parse_handshake_response(&out).unwrap();
    assert_eq!(parsed.username, username);
    assert_eq!(parsed.database.as_deref(), Some("main"));
    out
}

fn caching_sha2_token(password: &str, nonce: &[u8]) -> Vec<u8> {
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut digest = Sha256::new();
    digest.update(stage2);
    digest.update(nonce);
    let scramble = digest.finalize();
    stage1
        .iter()
        .zip(scramble.iter())
        .map(|(left, right)| left ^ right)
        .collect()
}
