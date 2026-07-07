CREATE TABLE symbol_info (
    symbol VARCHAR(64) PRIMARY KEY,
    name VARCHAR(128) NOT NULL,
    asset_type VARCHAR(32) NOT NULL,
    market VARCHAR(32) NOT NULL,
    exchange VARCHAR(32),
    currency VARCHAR(16),
    status VARCHAR(32) NOT NULL,
    timezone VARCHAR(64),
    tick_size DECIMAL(18, 8),
    lot_size DECIMAL(18, 8),
    listed_date DATE,
    delisted_date DATE,
    updated_at TIMESTAMP NOT NULL
);

CREATE TABLE market_min_kline (
    symbol VARCHAR(64) NOT NULL,
    trade_time TIMESTAMP NOT NULL,
    open DECIMAL(20, 8),
    high DECIMAL(20, 8),
    low DECIMAL(20, 8),
    close DECIMAL(20, 8),
    volume DECIMAL(28, 8),
    amount DECIMAL(28, 8),
    updated_at TIMESTAMP NOT NULL
)
PARTITION BY RANGE (trade_time)
WITH (
    partition_unit = 'day',
    retention = '30'
);
