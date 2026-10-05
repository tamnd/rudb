-- Makes oracle.tsv from the server at the pin:
--   psql -X -At -F "$(printf '\t')" -f oracle.sql > oracle.tsv
-- Each line is the type, extra_float_digits, the input, and the output text or the error.
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
-- The check is in EXECUTE because pg_input_is_valid caches the type when its argument looks
-- stable, and a parameter of a generic plan does.
create function pg_temp.out(t text, i text) returns text language plpgsql as $$
declare
  valid boolean;
  r text;
begin
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select ''ERROR '' || sql_error_code || '' '' || message from pg_input_error_info(%L, %L)', i, t)
      into r;
    return r;
  end if;
  -- format calls the output function of the type. A cast to text does not for every type: bool
  -- gives true and not t.
  execute format('select format(''%%s'', %L::%s)', i, t) into r;
  return r;
end
$$;
select t, 1, i, pg_temp.out(t, i) from inputs order by n;
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
