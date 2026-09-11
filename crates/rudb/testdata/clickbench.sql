-- The ClickBench queries, exactly as DuckDB runs them.
--
-- Copied from the suite in tamnd/rudb-bench, which copied them from the ClickBench
-- repository. Nothing here is rewritten for rudb. That is the point of the file: a query
-- that needs an edit to run is a compatibility bug, not a dialect difference.

CREATE TABLE hits (WatchID BIGINT NOT NULL, JavaEnable SMALLINT NOT NULL, Title TEXT, GoodEvent SMALLINT NOT NULL, EventTime TIMESTAMP NOT NULL, EventDate Date NOT NULL, CounterID INTEGER NOT NULL, ClientIP INTEGER NOT NULL, RegionID INTEGER NOT NULL, UserID BIGINT NOT NULL, CounterClass SMALLINT NOT NULL, OS SMALLINT NOT NULL, UserAgent SMALLINT NOT NULL, URL TEXT, Referer TEXT, IsRefresh SMALLINT NOT NULL, RefererCategoryID SMALLINT NOT NULL, RefererRegionID INTEGER NOT NULL, URLCategoryID SMALLINT NOT NULL, URLRegionID INTEGER NOT NULL, ResolutionWidth SMALLINT NOT NULL, ResolutionHeight SMALLINT NOT NULL, ResolutionDepth SMALLINT NOT NULL, FlashMajor SMALLINT NOT NULL, FlashMinor SMALLINT NOT NULL, FlashMinor2 TEXT, NetMajor SMALLINT NOT NULL, NetMinor SMALLINT NOT NULL, UserAgentMajor SMALLINT NOT NULL, UserAgentMinor VARCHAR(255) NOT NULL, CookieEnable SMALLINT NOT NULL, JavascriptEnable SMALLINT NOT NULL, IsMobile SMALLINT NOT NULL, MobilePhone SMALLINT NOT NULL, MobilePhoneModel TEXT, Params TEXT, IPNetworkID INTEGER NOT NULL, TraficSourceID SMALLINT NOT NULL, SearchEngineID SMALLINT NOT NULL, SearchPhrase TEXT, AdvEngineID SMALLINT NOT NULL, IsArtifical SMALLINT NOT NULL, WindowClientWidth SMALLINT NOT NULL, WindowClientHeight SMALLINT NOT NULL, ClientTimeZone SMALLINT NOT NULL, ClientEventTime TIMESTAMP NOT NULL, SilverlightVersion1 SMALLINT NOT NULL, SilverlightVersion2 SMALLINT NOT NULL, SilverlightVersion3 INTEGER NOT NULL, SilverlightVersion4 SMALLINT NOT NULL, PageCharset TEXT, CodeVersion INTEGER NOT NULL, IsLink SMALLINT NOT NULL, IsDownload SMALLINT NOT NULL, IsNotBounce SMALLINT NOT NULL, FUniqID BIGINT NOT NULL, OriginalURL TEXT, HID INTEGER NOT NULL, IsOldCounter SMALLINT NOT NULL, IsEvent SMALLINT NOT NULL, IsParameter SMALLINT NOT NULL, DontCountHits SMALLINT NOT NULL, WithHash SMALLINT NOT NULL, HitColor CHAR NOT NULL, LocalEventTime TIMESTAMP NOT NULL, Age SMALLINT NOT NULL, Sex SMALLINT NOT NULL, Income SMALLINT NOT NULL, Interests SMALLINT NOT NULL, Robotness SMALLINT NOT NULL, RemoteIP INTEGER NOT NULL, WindowName INTEGER NOT NULL, OpenerName INTEGER NOT NULL, HistoryLength SMALLINT NOT NULL, BrowserLanguage TEXT, BrowserCountry TEXT, SocialNetwork TEXT, SocialAction TEXT, HTTPError SMALLINT NOT NULL, SendTiming INTEGER NOT NULL, DNSTiming INTEGER NOT NULL, ConnectTiming INTEGER NOT NULL, ResponseStartTiming INTEGER NOT NULL, ResponseEndTiming INTEGER NOT NULL, FetchTiming INTEGER NOT NULL, SocialSourceNetworkID SMALLINT NOT NULL, SocialSourcePage TEXT, ParamPrice BIGINT NOT NULL, ParamOrderID TEXT, ParamCurrency TEXT, ParamCurrencyID SMALLINT NOT NULL, OpenstatServiceName TEXT, OpenstatCampaignID TEXT, OpenstatAdID TEXT, OpenstatSourceID TEXT, UTMSource TEXT, UTMMedium TEXT, UTMCampaign TEXT, UTMContent TEXT, UTMTerm TEXT, FromTag TEXT, HasGCLID SMALLINT NOT NULL, RefererHash BIGINT NOT NULL, URLHash BIGINT NOT NULL, CLID INTEGER NOT NULL);

-- q1
SELECT COUNT(*) FROM hits;

-- q2
SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0;

-- q3
SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits;

-- q4
SELECT AVG(UserID) FROM hits;

-- q5
SELECT COUNT(DISTINCT UserID) FROM hits;

-- q6
SELECT COUNT(DISTINCT SearchPhrase) FROM hits;

-- q7
SELECT MIN(EventDate), MAX(EventDate) FROM hits;

-- q8
SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY AdvEngineID ORDER BY COUNT(*) DESC;

-- q9
SELECT RegionID, COUNT(DISTINCT UserID) AS u FROM hits GROUP BY RegionID ORDER BY u DESC LIMIT 10;

-- q10
SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10;

-- q11
SELECT MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhoneModel ORDER BY u DESC LIMIT 10;

-- q12
SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhone, MobilePhoneModel ORDER BY u DESC LIMIT 10;

-- q13
SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;

-- q14
SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY u DESC LIMIT 10;

-- q15
SELECT SearchEngineID, SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, SearchPhrase ORDER BY c DESC LIMIT 10;

-- q16
SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10;

-- q17
SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10;

-- q18
SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase LIMIT 10;

-- q19
SELECT UserID, extract(minute FROM EventTime) AS m, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, m, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10;

-- q20
SELECT UserID FROM hits WHERE UserID = 435090932899640449;

-- q21
SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%';

-- q22
SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits WHERE URL LIKE '%google%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;

-- q23
SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, COUNT(DISTINCT UserID) FROM hits WHERE Title LIKE '%Google%' AND URL NOT LIKE '%.google.%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;

-- q24
SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10;

-- q25
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10;

-- q26
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY SearchPhrase LIMIT 10;

-- q27
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime, SearchPhrase LIMIT 10;

-- q28
SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits WHERE URL <> '' GROUP BY CounterID HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25;

-- q29
SELECT REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1') AS k, AVG(length(Referer)) AS l, COUNT(*) AS c, MIN(Referer) FROM hits WHERE Referer <> '' GROUP BY k HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25;

-- q30
SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), SUM(ResolutionWidth + 2), SUM(ResolutionWidth + 3), SUM(ResolutionWidth + 4), SUM(ResolutionWidth + 5), SUM(ResolutionWidth + 6), SUM(ResolutionWidth + 7), SUM(ResolutionWidth + 8), SUM(ResolutionWidth + 9), SUM(ResolutionWidth + 10), SUM(ResolutionWidth + 11), SUM(ResolutionWidth + 12), SUM(ResolutionWidth + 13), SUM(ResolutionWidth + 14), SUM(ResolutionWidth + 15), SUM(ResolutionWidth + 16), SUM(ResolutionWidth + 17), SUM(ResolutionWidth + 18), SUM(ResolutionWidth + 19), SUM(ResolutionWidth + 20), SUM(ResolutionWidth + 21), SUM(ResolutionWidth + 22), SUM(ResolutionWidth + 23), SUM(ResolutionWidth + 24), SUM(ResolutionWidth + 25), SUM(ResolutionWidth + 26), SUM(ResolutionWidth + 27), SUM(ResolutionWidth + 28), SUM(ResolutionWidth + 29), SUM(ResolutionWidth + 30), SUM(ResolutionWidth + 31), SUM(ResolutionWidth + 32), SUM(ResolutionWidth + 33), SUM(ResolutionWidth + 34), SUM(ResolutionWidth + 35), SUM(ResolutionWidth + 36), SUM(ResolutionWidth + 37), SUM(ResolutionWidth + 38), SUM(ResolutionWidth + 39), SUM(ResolutionWidth + 40), SUM(ResolutionWidth + 41), SUM(ResolutionWidth + 42), SUM(ResolutionWidth + 43), SUM(ResolutionWidth + 44), SUM(ResolutionWidth + 45), SUM(ResolutionWidth + 46), SUM(ResolutionWidth + 47), SUM(ResolutionWidth + 48), SUM(ResolutionWidth + 49), SUM(ResolutionWidth + 50), SUM(ResolutionWidth + 51), SUM(ResolutionWidth + 52), SUM(ResolutionWidth + 53), SUM(ResolutionWidth + 54), SUM(ResolutionWidth + 55), SUM(ResolutionWidth + 56), SUM(ResolutionWidth + 57), SUM(ResolutionWidth + 58), SUM(ResolutionWidth + 59), SUM(ResolutionWidth + 60), SUM(ResolutionWidth + 61), SUM(ResolutionWidth + 62), SUM(ResolutionWidth + 63), SUM(ResolutionWidth + 64), SUM(ResolutionWidth + 65), SUM(ResolutionWidth + 66), SUM(ResolutionWidth + 67), SUM(ResolutionWidth + 68), SUM(ResolutionWidth + 69), SUM(ResolutionWidth + 70), SUM(ResolutionWidth + 71), SUM(ResolutionWidth + 72), SUM(ResolutionWidth + 73), SUM(ResolutionWidth + 74), SUM(ResolutionWidth + 75), SUM(ResolutionWidth + 76), SUM(ResolutionWidth + 77), SUM(ResolutionWidth + 78), SUM(ResolutionWidth + 79), SUM(ResolutionWidth + 80), SUM(ResolutionWidth + 81), SUM(ResolutionWidth + 82), SUM(ResolutionWidth + 83), SUM(ResolutionWidth + 84), SUM(ResolutionWidth + 85), SUM(ResolutionWidth + 86), SUM(ResolutionWidth + 87), SUM(ResolutionWidth + 88), SUM(ResolutionWidth + 89) FROM hits;

-- q31
SELECT SearchEngineID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, ClientIP ORDER BY c DESC LIMIT 10;

-- q32
SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10;

-- q33
SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10;

-- q34
SELECT URL, COUNT(*) AS c FROM hits GROUP BY URL ORDER BY c DESC LIMIT 10;

-- q35
SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL ORDER BY c DESC LIMIT 10;

-- q36
SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c FROM hits GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3 ORDER BY c DESC LIMIT 10;

-- q37
SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND URL <> '' GROUP BY URL ORDER BY PageViews DESC LIMIT 10;

-- q38
SELECT Title, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND Title <> '' GROUP BY Title ORDER BY PageViews DESC LIMIT 10;

-- q39
SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 AND IsLink <> 0 AND IsDownload = 0 GROUP BY URL ORDER BY PageViews DESC LIMIT 10 OFFSET 1000;

-- q40
SELECT TraficSourceID, SearchEngineID, AdvEngineID, CASE WHEN (SearchEngineID = 0 AND AdvEngineID = 0) THEN Referer ELSE '' END AS Src, URL AS Dst, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 GROUP BY TraficSourceID, SearchEngineID, AdvEngineID, Src, Dst ORDER BY PageViews DESC LIMIT 10 OFFSET 1000;

-- q41
SELECT URLHash, EventDate, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 AND TraficSourceID IN (-1, 6) AND RefererHash = 3594120000172545465 GROUP BY URLHash, EventDate ORDER BY PageViews DESC LIMIT 10 OFFSET 100;

-- q42
SELECT WindowClientWidth, WindowClientHeight, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0 AND DontCountHits = 0 AND URLHash = 2868770270353813622 GROUP BY WindowClientWidth, WindowClientHeight ORDER BY PageViews DESC LIMIT 10 OFFSET 10000;

-- q43
SELECT DATE_TRUNC('minute', EventTime) AS M, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-14' AND EventDate <= '2013-07-15' AND IsRefresh = 0 AND DontCountHits = 0 GROUP BY DATE_TRUNC('minute', EventTime) ORDER BY DATE_TRUNC('minute', EventTime) LIMIT 10 OFFSET 1000;
