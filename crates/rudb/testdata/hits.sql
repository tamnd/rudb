-- How hits.parquet in this directory was made.
--
-- The ClickBench queries run against a file rather than against a hand written table, because that
-- is how the benchmark is written and because a file is the only thing that puts the Parquet reader
-- under the query engine. The published hits.parquet is fourteen gigabytes and nobody commits that,
-- so this is the same 105 columns with the same types in the same order and ten thousand rows of
-- data shaped to reach the queries.
--
-- Two derived columns do the shaping. b is the number of trailing zero bits of i + 1, so b is 0 for
-- half the rows, 1 for a quarter, 2 for an eighth and so on down to 13. Every group key the
-- benchmark groups on is built out of b, which means every group has a different number of rows in
-- it, which means ORDER BY count DESC LIMIT 10 has exactly one right answer. A fixture of uniform
-- keys would tie on every one of those queries and a tie plus a LIMIT is a result that two correct
-- engines are allowed to disagree about, so the comparison against duckdb would prove nothing. The
-- tail of b, where the groups get down to single rows, is folded back into group zero rather than
-- capped, because capping makes the last group exactly the size of the one before it, which is the
-- tie this is all trying to avoid. c is the same count over a multiplied i, which gives a second
-- skewed key that does not line up with the first, so that COUNT(DISTINCT UserID) per RegionID is a
-- different number in every region.
--
-- The rest is arranged so the filters remove something: CounterID is 62 for two rows in three,
-- EventDate covers all of July 2013, SearchPhrase and URL and Title are empty for whole groups,
-- some URLs hold google and some hold .google. so that q23's two LIKEs disagree, and UserID,
-- RefererHash and URLHash take the literal values q20, q41 and q42 look for. Thirteen of the
-- nullable text columns are null for some of their rows. ClickBench's own file has no nulls in it
-- at all, and a fixture with none either would never once exercise the null paths that a file from
-- anywhere else would take.
--
-- The four queries with a large OFFSET come back empty, because ten thousand rows over keys this
-- skewed do not make a thousand groups. That is the same empty that duckdb returns and the test
-- still compares it, but it is not evidence about those queries and the real file is what settles
-- them.
--
-- Regenerate with duckdb from this directory:
--
--   duckdb -c ".read hits.sql"
--
-- The expected answers next to it come from the same duckdb over the same file, so regenerating one
-- without the other is a test that measures nothing.

COPY (
    SELECT
        (b * 7919 + 1000000)::BIGINT AS WatchID,
        (i % 4)::SMALLINT AS JavaEnable,
        (CASE WHEN b = 0 THEN 'Google News' WHEN b = 1 THEN '' ELSE 'Page title ' || b END) AS Title,
        (i % 3)::SMALLINT AS GoodEvent,
        (TIMESTAMP '2013-07-14 00:00:00' + INTERVAL (i * 37) SECOND) AS EventTime,
        (DATE '2013-07-01' + (i % 31)::INTEGER) AS EventDate,
        (CASE WHEN i % 3 <> 2 THEN 62 ELSE (i % 100) + 1 END)::INTEGER AS CounterID,
        (b + 1000)::INTEGER AS ClientIP,
        b::INTEGER AS RegionID,
        (CASE WHEN g = 9 THEN 435090932899640449 ELSE g::BIGINT * 1000003 END)::BIGINT AS UserID,
        (i % 6)::SMALLINT AS CounterClass,
        (i % 5)::SMALLINT AS OS,
        (i % 3)::SMALLINT AS UserAgent,
        (CASE WHEN b = 0 THEN 'http://www.google.com/search?q=1' WHEN b = 1 THEN 'http://news.google.ru/page/2' WHEN b = 2 THEN '' WHEN b = 3 THEN 'http://googlemaps.example.com/3' ELSE 'http://example.com/' || b END) AS URL,
        (CASE WHEN b = 0 THEN '' ELSE 'http://www.site' || b || '.com/path/' || (i % 5) END) AS Referer,
        (CASE WHEN i % 9 = 0 THEN 1 ELSE 0 END)::SMALLINT AS IsRefresh,
        (i % 11)::SMALLINT AS RefererCategoryID,
        (i % 115)::INTEGER AS RefererRegionID,
        (i % 7)::SMALLINT AS URLCategoryID,
        (i % 111)::INTEGER AS URLRegionID,
        ((i % 1000) + 200)::SMALLINT AS ResolutionWidth,
        ((i % 800) + 150)::SMALLINT AS ResolutionHeight,
        (i % 9)::SMALLINT AS ResolutionDepth,
        (i % 4)::SMALLINT AS FlashMajor,
        (i % 4)::SMALLINT AS FlashMinor,
        ('flashminor2 ' || (i % 7)) AS FlashMinor2,
        (i % 11)::SMALLINT AS NetMajor,
        (i % 11)::SMALLINT AS NetMinor,
        (i % 8)::SMALLINT AS UserAgentMajor,
        (CASE WHEN i % 3 = 0 THEN 'ab' ELSE 'cd' END) AS UserAgentMinor,
        (i % 6)::SMALLINT AS CookieEnable,
        (i % 10)::SMALLINT AS JavascriptEnable,
        (i % 11)::SMALLINT AS IsMobile,
        (CASE WHEN b >= 5 THEN 0 ELSE b END)::SMALLINT AS MobilePhone,
        (CASE WHEN b = 0 OR b >= 6 THEN '' ELSE 'model ' || b END) AS MobilePhoneModel,
        (CASE WHEN i % 4 = 0 THEN NULL ELSE 'a=' || (i % 6) END) AS Params,
        (i % 111)::INTEGER AS IPNetworkID,
        (CASE WHEN b = 0 THEN -1 WHEN b = 1 THEN 6 WHEN b >= 6 THEN 0 ELSE b END)::SMALLINT AS TraficSourceID,
        (CASE WHEN b >= 4 THEN 0 ELSE b END)::SMALLINT AS SearchEngineID,
        (CASE WHEN b = 0 OR b >= 7 THEN '' ELSE 'phrase ' || b END) AS SearchPhrase,
        (CASE WHEN b >= 6 THEN 0 ELSE b END)::SMALLINT AS AdvEngineID,
        (i % 5)::SMALLINT AS IsArtifical,
        ((b * 10) + 300)::SMALLINT AS WindowClientWidth,
        ((b * 10) + 200)::SMALLINT AS WindowClientHeight,
        (i % 8)::SMALLINT AS ClientTimeZone,
        (TIMESTAMP '2013-07-14 00:00:00' + INTERVAL (i * 41) SECOND) AS ClientEventTime,
        (i % 4)::SMALLINT AS SilverlightVersion1,
        (i % 4)::SMALLINT AS SilverlightVersion2,
        (i % 119)::INTEGER AS SilverlightVersion3,
        (i % 4)::SMALLINT AS SilverlightVersion4,
        ('pagecharset ' || (i % 7)) AS PageCharset,
        (i % 111)::INTEGER AS CodeVersion,
        (CASE WHEN i % 4 = 0 THEN 1 ELSE 0 END)::SMALLINT AS IsLink,
        (CASE WHEN i % 23 = 0 THEN 1 ELSE 0 END)::SMALLINT AS IsDownload,
        (i % 5)::SMALLINT AS IsNotBounce,
        (i * 7926)::BIGINT AS FUniqID,
        (CASE WHEN i % 3 = 0 THEN NULL ELSE 'http://origin.example.com/' || (i % 7) END) AS OriginalURL,
        (i % 103)::INTEGER AS HID,
        (i % 6)::SMALLINT AS IsOldCounter,
        (i % 10)::SMALLINT AS IsEvent,
        (i % 5)::SMALLINT AS IsParameter,
        (CASE WHEN i % 5 = 0 THEN 1 ELSE 0 END)::SMALLINT AS DontCountHits,
        (i % 11)::SMALLINT AS WithHash,
        (CASE WHEN i % 2 = 0 THEN 'a' ELSE 'b' END) AS HitColor,
        (TIMESTAMP '2013-07-14 00:00:00' + INTERVAL (i * 43) SECOND) AS LocalEventTime,
        (i % 6)::SMALLINT AS Age,
        (i % 6)::SMALLINT AS Sex,
        (i % 9)::SMALLINT AS Income,
        (i % 3)::SMALLINT AS Interests,
        (i % 3)::SMALLINT AS Robotness,
        (i % 108)::INTEGER AS RemoteIP,
        (i % 110)::INTEGER AS WindowName,
        (i % 110)::INTEGER AS OpenerName,
        (i % 7)::SMALLINT AS HistoryLength,
        ('browserlanguage ' || (i % 4)) AS BrowserLanguage,
        ('browsercountry ' || (i % 3)) AS BrowserCountry,
        ('socialnetwork ' || (i % 9)) AS SocialNetwork,
        ('socialaction ' || (i % 8)) AS SocialAction,
        (i % 3)::SMALLINT AS HTTPError,
        (i % 110)::INTEGER AS SendTiming,
        (i % 109)::INTEGER AS DNSTiming,
        (i % 113)::INTEGER AS ConnectTiming,
        (i % 119)::INTEGER AS ResponseStartTiming,
        (i % 117)::INTEGER AS ResponseEndTiming,
        (i % 111)::INTEGER AS FetchTiming,
        (i % 6)::SMALLINT AS SocialSourceNetworkID,
        (CASE WHEN i % 6 = 0 THEN NULL ELSE '' END) AS SocialSourcePage,
        (i * 7929)::BIGINT AS ParamPrice,
        (CASE WHEN i % 2 = 0 THEN NULL ELSE '' END) AS ParamOrderID,
        (CASE WHEN i % 3 = 0 THEN NULL ELSE '' END) AS ParamCurrency,
        (i % 9)::SMALLINT AS ParamCurrencyID,
        (CASE WHEN i % 2 = 0 THEN NULL ELSE '' END) AS OpenstatServiceName,
        (CASE WHEN i % 3 = 0 THEN NULL ELSE '' END) AS OpenstatCampaignID,
        (CASE WHEN i % 4 = 0 THEN NULL ELSE '' END) AS OpenstatAdID,
        (CASE WHEN i % 5 = 0 THEN NULL ELSE '' END) AS OpenstatSourceID,
        (CASE WHEN i % 2 = 0 THEN NULL ELSE '' END) AS UTMSource,
        (CASE WHEN i % 3 = 0 THEN NULL ELSE '' END) AS UTMMedium,
        (CASE WHEN i % 4 = 0 THEN NULL ELSE '' END) AS UTMCampaign,
        (CASE WHEN i % 5 = 0 THEN NULL ELSE '' END) AS UTMContent,
        (CASE WHEN i % 6 = 0 THEN NULL ELSE '' END) AS UTMTerm,
        (CASE WHEN i % 4 = 0 THEN NULL ELSE '' END) AS FromTag,
        (i % 11)::SMALLINT AS HasGCLID,
        (CASE WHEN b = 1 THEN 3594120000172545465 ELSE i * 31 END)::BIGINT AS RefererHash,
        (CASE WHEN b = 0 THEN 2868770270353813622 ELSE i * 17 END)::BIGINT AS URLHash,
        (i % 104)::INTEGER AS CLID
    FROM (
        SELECT
            i,
            CASE
                WHEN (i + 1) % 2 = 1 THEN 0
                WHEN (i + 1) % 4 = 2 THEN 1
                WHEN (i + 1) % 8 = 4 THEN 2
                WHEN (i + 1) % 16 = 8 THEN 3
                WHEN (i + 1) % 32 = 16 THEN 4
                WHEN (i + 1) % 64 = 32 THEN 5
                WHEN (i + 1) % 128 = 64 THEN 6
                WHEN (i + 1) % 256 = 128 THEN 7
                WHEN (i + 1) % 512 = 256 THEN 8
                WHEN (i + 1) % 1024 = 512 THEN 9
                WHEN (i + 1) % 2048 = 1024 THEN 10
                WHEN (i + 1) % 4096 = 2048 THEN 11
                WHEN (i + 1) % 8192 = 4096 THEN 12
                WHEN (i + 1) % 16384 = 8192 THEN 13
                ELSE 14
            END AS b,
            CASE
                WHEN (i * 2654435761 % 10007 + 1) % 2 = 1 THEN 0
                WHEN (i * 2654435761 % 10007 + 1) % 4 = 2 THEN 1
                WHEN (i * 2654435761 % 10007 + 1) % 8 = 4 THEN 2
                WHEN (i * 2654435761 % 10007 + 1) % 16 = 8 THEN 3
                WHEN (i * 2654435761 % 10007 + 1) % 32 = 16 THEN 4
                WHEN (i * 2654435761 % 10007 + 1) % 64 = 32 THEN 5
                WHEN (i * 2654435761 % 10007 + 1) % 128 = 64 THEN 6
                WHEN (i * 2654435761 % 10007 + 1) % 256 = 128 THEN 7
                WHEN (i * 2654435761 % 10007 + 1) % 512 = 256 THEN 8
                WHEN (i * 2654435761 % 10007 + 1) % 1024 = 512 THEN 9
                WHEN (i * 2654435761 % 10007 + 1) % 2048 = 1024 THEN 10
                WHEN (i * 2654435761 % 10007 + 1) % 4096 = 2048 THEN 11
                WHEN (i * 2654435761 % 10007 + 1) % 8192 = 4096 THEN 12
                WHEN (i * 2654435761 % 10007 + 1) % 16384 = 8192 THEN 13
                ELSE 14
            END AS c,
            floor((sqrt(8 * i + 1) - 1) / 2)::INTEGER AS g
        FROM range(10000) t(i)
    )
) TO 'hits.parquet' (FORMAT PARQUET, COMPRESSION SNAPPY, ROW_GROUP_SIZE 2048);
