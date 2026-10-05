-- Makes oracle.tsv from the server at the pin:
--   psql -X -At -F "$(printf '\t')" -f oracle.sql > oracle.tsv
-- Each line is the type, extra_float_digits, the input, and the output text or the error. A type
-- that starts with "send" has the hex of the binary output.
\set QUIET on
create temp table inputs (n serial, t text, i text);
insert into inputs (t, i) values
  ('int2', '32767'), ('int2', '-32768'), ('int2', '32768'), ('int2', '0x7FFF'), ('int2', ' 1_2 '),
  ('int4', '0'), ('int4', '-0'), ('int4', ' 12 '), ('int4', '+7'), ('int4', '0x1F'), ('int4', '-0X1f'),
  ('int4', '0o17'), ('int4', '0b101'), ('int4', '1_000'), ('int4', '0x_1F'), ('int4', '2147483647'),
  ('int4', '-2147483648'), ('int4', '-0x80000000'), ('int4', ''), ('int4', ' '), ('int4', '1e3'),
  ('int4', '1.5'), ('int4', '_1'), ('int4', '1_'), ('int4', '1__0'), ('int4', '0x'), ('int4', '0b2'),
  ('int4', '- 1'), ('int4', '1 2'), ('int4', '0o8'), ('int4', '2147483648'), ('int4', '-2147483649'),
  ('int4', '0x80000000'), ('int4', ' 99999999999 '), ('int4', '99999999999x'), ('int4', '00012'),
  ('int8', '-9223372036854775808'), ('int8', '9223372036854775808'), ('int8', '0x7FFF_FFFF_FFFF_FFFF'),
  ('int8', 'x'), ('int8', '0b1111111111111111111111111111111111111111111111111111111111111111'),
  ('oid', '0'), ('oid', '16384'), ('oid', ' 010 '), ('oid', '0x10'), ('oid', '-1'), ('oid', '-2147483648'),
  ('oid', '4294967295'), ('oid', '+5'), ('oid', ''), ('oid', 'x'), ('oid', '0x'), ('oid', '08'),
  ('oid', '1_0'), ('oid', '1.0'), ('oid', '-'), ('oid', '4294967296'), ('oid', '-2147483649'),
  ('oid', '99999999999999999999999'), ('oid', '0X'), ('oid', '0xg'),
  ('bool', 't'), ('bool', 'true'), ('bool', 'TRUE'), ('bool', ' yes '), ('bool', 'y'), ('bool', 'on'),
  ('bool', 'o'), ('bool', 'of'), ('bool', 'off'), ('bool', '1'), ('bool', '0'), ('bool', 'f'),
  ('bool', 'fa'), ('bool', 'no'), ('bool', 'n'), ('bool', ''), ('bool', '2'), ('bool', 'tru e'),
  ('bool', 'offx'), ('bool', 'truex'),
  ('"char"', 'a'), ('"char"', 'abc'), ('"char"', ''), ('"char"', '\101'), ('"char"', '\377'), ('"char"', '\'),
  ('"char"', '\12'), ('"char"', 'é'),
  ('name', 'abc'), ('name', repeat('n', 63)), ('name', repeat('n', 64)), ('name', repeat('é', 40)),
  ('bytea', '\x'), ('bytea', '\x0a0B'), ('bytea', '\x 0a 0b '), ('bytea', '\x0g'), ('bytea', '\x123'),
  ('bytea', '\X0a'), ('bytea', 'abc'), ('bytea', 'a\\b'), ('bytea', '\141\142'), ('bytea', '\q'),
  ('bytea', '\1'), ('bytea', '\400'), ('bytea', 'é'), ('bytea', '\x0 a'),
  ('uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'), ('uuid', 'A0EEBC999C0B4EF8BB6D6BB9BD380A11'),
  ('uuid', '{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}'), ('uuid', 'a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11'),
  ('uuid', '{a0eebc999c0b4ef8bb6d6bb9bd380a11'), ('uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1'),
  ('uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 '), ('uuid', '-a0eebc999c0b4ef8bb6d6bb9bd380a11');
create temp table floats (n serial, t text, i text);
insert into floats (t, i) select t, i from
  (values ('float8'), ('float4')) as types (t),
  (values ('0'), ('-0'), ('1'), ('-1.5'), ('0.1'), ('2.5'), ('100'), ('123456'), ('1234567'),
    ('1e6'), ('1e7'), ('1e14'), ('1e15'), ('1e16'), ('1e22'), ('123456789012345678'), ('0.0001'),
    ('0.00001'), ('1.5e-5'), ('0.000123'), ('3.141592653589793'), ('2.718281828459045'),
    ('1.7976931348623157e308'), ('2.2250738585072014e-308'), ('4.9e-324'), ('5e-324'), ('1e-320'),
    ('3.4028235e38'), ('3.5e38'), ('1.17549435e-38'), ('1.4e-45'), ('1e-46'), ('1e-50'), ('1e39'),
    ('1e309'), ('-1e309'), ('1e-400'), ('0e-400'), ('0x1p-1074'), ('0x1.8p1'), ('0X10'), ('0x'),
    ('NaN'), ('nan'), ('Infinity'), ('-infinity'), ('inf'), ('+Inf'), ('-INF'), ('infinit'),
    (' 1.5 '), ('1.5x'), (''), (' '), ('.5'), ('5.'), ('.'), ('1e'), ('1e+'), ('e5'), ('+-1'),
    ('1_000'), ('9007199254740993'), ('0.30000000000000004'), ('1e-7'), ('123456.7'),
    ('16777217'), ('0.333333333333333333')) as inputs (i);
create temp table numerics (n serial, t text, i text);
insert into numerics (t, i) select 'numeric', i from unnest(array[
  '0', '-0', '0.000', '-0.000', '1', '-1', '+1', ' 12.5 ', '12.50', '.5', '5.', '.', '', ' ', '1e3',
  '1E-3', '1.5e+2', '1e', '1e+', 'e5', '1.2.3', '1_000.000_1', '1_', '_1', '1._5', '1_.5', '1__0',
  '1e_1', '1e1_0', '1.e5', '.e5', '0x1F', '-0x1f', '+0o17', '0b101', '0x_1F', '0x', '0b2', '0x1.5',
  '0x1_', '0xFFFFFFFFFFFFFFFF', '0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF', '-0o777777777777777777777777',
  '0b' || repeat('1', 130), 'NaN', 'nan', '-NaN', '+NaN', ' NaN ', 'Infinity', '-Infinity', 'inf',
  '+INF', '-inf', 'infinit', 'infinityx', 'NaNx', 'abc', '1 2', '- 1', '+-1', '1.5x',
  '123456789012345678901234567890.123456789012345678901234567890', '0.00000000000000000001',
  '1e-20', '1e-16384', '1e131072', '1e1073741823', '1e1073741824', '9999.99995', '0.0001',
  '0.00012345', '100000000', '99999999.99999999', '1e100', '-1.5e-10', '00012.3400',
  '12345678901234567890', '170141183460469231731687303715884105727',
  '-170141183460469231731687303715884105727', '1.70141183460469231731687303715884105727',
  '0.12345678901234567890123456789012345678', '123456789.123456789', '1e38', '1e39', '5e-38',
  '0.000000000000000000000000000000000000001'
]) as i;
insert into numerics (t, i) values
  ('numeric(5,2)', '123.456'), ('numeric(5,2)', '123.455'), ('numeric(5,2)', '-123.455'),
  ('numeric(5,2)', '999.994'), ('numeric(5,2)', '999.995'), ('numeric(5,2)', '0.001'),
  ('numeric(5,2)', '0.005'), ('numeric(5,2)', '-0.005'), ('numeric(5,2)', '-0.004'),
  ('numeric(5,2)', 'NaN'), ('numeric(5,2)', 'Infinity'), ('numeric(5,2)', '-inf'),
  ('numeric(5,2)', '1e2'), ('numeric(5,2)', '12345'), ('numeric(5,2)', 'abc'),
  ('numeric(5,2)', '1e1073741824'), ('numeric(5,2)', '1e-16384'), ('numeric(5,2)', '1e131072'),
  ('numeric(5,2)', '0x10'), ('numeric(5,2)', '0xFFFFFFFF'), ('numeric(5,2)', 'infx'),
  ('numeric(3,-1)', '15'), ('numeric(3,-1)', '14'), ('numeric(3,-1)', '-15'),
  ('numeric(3,-1)', '9994'), ('numeric(3,-1)', '9995'), ('numeric(3,-1)', '0'),
  ('numeric(3,-1)', '4.9'), ('numeric(2,5)', '0.000123'), ('numeric(2,5)', '0.0001234'),
  ('numeric(2,5)', '0.001'), ('numeric(2,5)', '0.000995'), ('numeric(2,5)', '0.000994'),
  ('numeric(1,0)', '0.5'), ('numeric(1,0)', '9.4'), ('numeric(1,0)', '9.5'), ('numeric(1,0)', '-9.5'),
  ('numeric(1,1)', '0.95'), ('numeric(1,1)', '0.94'), ('numeric(1,1)', '-0.04'),
  ('numeric(1,1)', '-0.05'), ('numeric(1000,1000)', '0.5'), ('numeric(10,4)', '9999.99995'),
  ('numeric(8,4)', '9999.99995'), ('numeric(8,4)', '0.00005'), ('numeric(4,-3)', '1234567'),
  ('numeric(4,-3)', '12345678'), ('numeric(4,-3)', '9999499.9'), ('numeric(4,-3)', '9999500'),
  ('numeric(38,0)', '99999999999999999999999999999999999999'), ('numeric(38,0)', '1e38'),
  ('numeric(38,38)', '0.99999999999999999999999999999999999999'), ('numeric(38,38)', '1');
-- The check is in EXECUTE because pg_input_is_valid caches the type when its argument looks
-- stable, and a parameter of a generic plan does.
create function pg_temp.out(t text, i text) returns text language plpgsql as $$
declare
  valid boolean;
  r text;
begin
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select ''ERROR '' || sql_error_code || '' '' || message || coalesce('' DETAIL '' || detail, '''') from pg_input_error_info(%L, %L)', i, t)
      into r;
    return r;
  end if;
  -- A cast of a literal to numeric(p,s) reads the literal with no typmod and then calls the
  -- numeric function, which can fail where the input function with the typmod does not. So the
  -- numeric types call the input function.
  if t like 'numeric%' then
    return numeric_out(numeric_in(i::cstring, 0, to_regtypemod(t)));
  end if;
  -- format calls the output function of the type. A cast to text does not for every type: bool
  -- gives true and not t.
  execute format('select format(''%%s'', %L::%s)', i, t) into r;
  return r;
end
$$;
create function pg_temp.send(t text, i text) returns text language sql
  return encode(numeric_send(numeric_in(i::cstring, 0, to_regtypemod(t))), 'hex');
select t, 1, i, pg_temp.out(t, i) from inputs order by n;
select t, 1, i, pg_temp.out(t, i) from numerics order by n;
select 'send ' || t, 1, i, pg_temp.send(t, i) from numerics where pg_temp.out(t, i) not like 'ERROR %' order by n;
set extra_float_digits = 1;
select t, 1, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = 3;
select t, 3, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = 0;
select t, 0, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = 2;
select t, 2, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = -3;
select t, -3, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = -15;
select t, -15, i, pg_temp.out(t, i) from floats order by n;
-- The date and time types. Each value is read once in ISO and UTC, and the line has the hex of
-- the binary output as the input, so that the test checks the receive function and the output
-- function together. The second field is DateStyle, DateStyle and TimeZone, or IntervalStyle.
reset datestyle;
set timezone = 'UTC';
create temp table dates (n serial, v date);
insert into dates (v) values ('2026-10-05'), ('2000-01-01'), ('1999-12-31'), ('1970-01-01'),
  ('2000-02-29'), ('1900-03-01'), ('0001-01-01'), ('0001-12-31 BC'), ('0044-03-15 BC'),
  ('4714-11-24 BC'), ('9999-12-31'), ('10000-01-01'), ('5874897-12-31'), ('infinity'),
  ('-infinity');
create temp table times (n serial, v time);
insert into times (v) values ('00:00'), ('24:00'), ('12:34:56.789'), ('23:59:59.999999'),
  ('00:00:00.000001'), ('01:02:03.1'), ('10:00:00.120');
create temp table timetzs (n serial, v timetz);
insert into timetzs (v) values ('12:34:56+05:30'), ('00:00-15:59'), ('24:00+15:59:59'), ('12:00+00'),
  ('12:00-05:21:10'), ('23:59:59.5+01'), ('06:00:00.000001-00:00:01');
create temp table stamps (n serial, v timestamp);
insert into stamps (v) values ('2026-10-04 12:34:56.789'), ('2026-10-05 00:00'), ('2026-10-06 01:00'),
  ('2026-10-07 23:59:59.999999'), ('2026-10-08 00:00:00.000001'), ('2026-10-09 10:00:00.5'),
  ('2026-10-10 12:00'), ('2000-01-01 00:00'), ('1999-12-31 23:59:59.5'), ('4714-11-24 00:00 BC'),
  ('0044-03-15 12:00 BC'), ('0001-01-01 00:00'), ('1900-01-01 00:00'), ('10000-01-01 00:00'),
  ('294276-12-31 23:59:59.999999'), ('infinity'), ('-infinity');
create temp table stamptzs as select n, v::timestamptz as v from stamps;
create temp table intervals (n serial, v interval);
insert into intervals (v) values ('0'), ('1 year 2 months 3 days 4:05:06'),
  ('-1 year -2 months +3 days -4:05:06'), ('1 day'), ('-1 day'), ('1 sec'), ('-1 sec'), ('1.5 sec'),
  ('-0.5 sec'), ('0.000001 sec'), ('1 mon'), ('-1 mon'), ('1 year'), ('2 years'), ('-1 year'),
  ('25 hours'), ('-25:00:00.000001'), ('1 min'), ('-1 min'), ('1 hour 1 min'), ('3 days 0:00:01'),
  ('1 day -1 sec'), ('-1 day +1 sec'), ('1 year -1 day'), ('-1 year 1 day'), ('1 mon 1 day 00:00:00.5'),
  ('-1 mon -1 day -00:00:00.5'), ('178956970 years 7 months'), ('-178956970 years -8 months'),
  ('2147483647 days'), ('-2147483648 days'), ('2562047788 hours'), ('-2562047788 hours'),
  ('1 year 1 sec'), ('-1 sec 1 year'), ('1 day 1 sec'), ('-1 day -1 sec'), ('10 days -10 sec'),
  ('infinity'), ('-infinity');
create function pg_temp.datetimes() returns table (t text, s text, i text, o text)
language plpgsql as $$
declare
  ds text;
  tz text;
  st text;
begin
  foreach ds in array array['ISO, MDY', 'ISO, DMY', 'SQL, MDY', 'SQL, DMY', 'SQL, YMD',
    'Postgres, MDY', 'Postgres, DMY', 'Postgres, YMD', 'German, DMY'] loop
    perform set_config('datestyle', ds, false);
    return query select 'date', ds, encode(date_send(v), 'hex'), v::text from dates order by n;
    return query select 'timestamp', ds, encode(timestamp_send(v), 'hex'), v::text from stamps order by n;
    foreach tz in array array['UTC', '<+05:30>-05:30', '+05:30'] loop
      perform set_config('timezone', tz, false);
      return query select 'timestamptz', ds || '|' || tz, encode(timestamptz_send(v), 'hex'), v::text
        from stamptzs order by n;
    end loop;
    perform set_config('timezone', 'UTC', false);
  end loop;
  perform set_config('datestyle', 'ISO, MDY', false);
  return query select 'time', 'ISO, MDY', encode(time_send(v), 'hex'), v::text from times order by n;
  return query select 'timetz', 'ISO, MDY', encode(timetz_send(v), 'hex'), v::text from timetzs order by n;
  foreach st in array array['postgres', 'postgres_verbose', 'sql_standard', 'iso_8601'] loop
    perform set_config('intervalstyle', st, false);
    return query select 'interval', st, encode(interval_send(v), 'hex'), v::text from intervals order by n;
  end loop;
  perform set_config('intervalstyle', 'postgres', false);
end
$$;
select * from pg_temp.datetimes();
