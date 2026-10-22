use tableport_core::{parse_connection_url, Engine, ParsedUrl, TlsMode};

fn some(text: &str) -> Option<String> {
    Some(text.to_string())
}

#[test]
fn full_url() {
    let parsed = parse_connection_url("postgres://app:p%40ss%3Aword@db.example.com:6543/shop?sslmode=require").unwrap();
    assert_eq!(
        parsed,
        ParsedUrl {
            host: some("db.example.com"),
            port: Some(6543),
            database: some("shop"),
            user: some("app"),
            password: some("p@ss:word"),
            tls: Some(TlsMode::Require),
            engine: Engine::Postgres,
        }
    );
}

#[test]
fn mysql_and_mariadb_urls() {
    let parsed = parse_connection_url("mysql://root:secret@db.example.com/shop?ssl-mode=REQUIRED").unwrap();
    assert_eq!(parsed.engine, Engine::Mysql);
    assert_eq!(parsed.port, Some(3306), "MySQL's default port");
    assert_eq!(parsed.database, some("shop"));
    assert_eq!(parsed.tls, Some(TlsMode::Require));
    let parsed = parse_connection_url("mariadb://app@127.0.0.1:3308?ssl-mode=VERIFY_IDENTITY").unwrap();
    assert_eq!((parsed.engine, parsed.port, parsed.database), (Engine::Mysql, Some(3308), None));
    assert_eq!(parsed.tls, Some(TlsMode::VerifyFull));
}

#[test]
fn postgresql_scheme_and_missing_parts() {
    let parsed = parse_connection_url("  postgresql://localhost  ").unwrap();
    assert_eq!(parsed.host, some("localhost"));
    // No port in the URL means PostgreSQL's default.
    assert_eq!(parsed.port, Some(5432));
    assert_eq!(parsed.user, None);
    assert_eq!(parsed.password, None);
    assert_eq!(parsed.database, None);
    assert_eq!(parsed.tls, None);
}

#[test]
fn verifying_sslmodes_mean_verify_full() {
    let parsed = parse_connection_url("postgres://u@h/d?sslmode=verify-full&application_name=x").unwrap();
    assert_eq!(parsed.tls, Some(TlsMode::VerifyFull));
    assert_eq!(parse_connection_url("postgres://u@h/d?sslmode=verify-ca").unwrap().tls, Some(TlsMode::VerifyFull));
    assert_eq!(parse_connection_url("postgres://u@h/d?sslmode=disable").unwrap().tls, Some(TlsMode::Disable));
}

#[test]
fn supabase_pooler_url() {
    let parsed = parse_connection_url(
        "postgresql://postgres.abcdefghijk:secret@aws-1-ap-south-1.pooler.supabase.com:6543/postgres",
    )
    .unwrap();
    assert_eq!(parsed.user, some("postgres.abcdefghijk"));
    assert_eq!(parsed.host, some("aws-1-ap-south-1.pooler.supabase.com"));
    assert_eq!(parsed.port, Some(6543));
}

#[test]
fn other_text_is_refused() {
    let error = parse_connection_url("mongodb://root@localhost/db").unwrap_err();
    assert!(error.message().contains("postgres://"), "{error}");
    let error = parse_connection_url("postgres://u@h:notaport/d").unwrap_err();
    assert!(error.message().contains("can't be read"), "{error}");
}

#[test]
fn unknown_parameters_are_skipped() {
    // Prisma and pooler URLs, which tokio-postgres' parser refused.
    let parsed = parse_connection_url(
        "postgresql://app_user:s3cret@db-host.example.com:25060/app_dev?schema=public&connection_limit=5&pgbouncer=true&sslmode=require",
    )
    .unwrap();
    assert_eq!(parsed.user, some("app_user"));
    assert_eq!(parsed.password, some("s3cret"));
    assert_eq!(parsed.host, some("db-host.example.com"));
    assert_eq!(parsed.port, Some(25060));
    assert_eq!(parsed.database, some("app_dev"));
    assert_eq!(parsed.tls, Some(TlsMode::Require));
}

#[test]
fn heroku_no_verify_and_neon_channel_binding() {
    assert_eq!(parse_connection_url("postgres://u:p@h/d?sslmode=no-verify").unwrap().tls, Some(TlsMode::Require));
    let neon = parse_connection_url("postgresql://u:p@ep-x.aws.neon.tech/neondb?sslmode=require&channel_binding=require").unwrap();
    assert_eq!(neon.tls, Some(TlsMode::Require));
    assert_eq!(neon.database, some("neondb"));
}

#[test]
fn awkward_hosts_and_passwords() {
    let parsed = parse_connection_url("postgres://u:p@ss@[::1]:6000/d").unwrap();
    assert_eq!(parsed.password, some("p@ss"), "an unencoded @ in the password");
    assert_eq!(parsed.host, some("::1"));
    assert_eq!(parsed.port, Some(6000));
    let failover = parse_connection_url("postgres://u@primary:5432,replica:5433/d").unwrap();
    assert_eq!(failover.host, some("primary"));
    let params = parse_connection_url("postgres:///d?host=db.internal&port=7000&user=me").unwrap();
    assert_eq!((params.host, params.port, params.user), (some("db.internal"), Some(7000), some("me")));
}
