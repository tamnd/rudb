fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: certify_grouped_distinct FILE TABLE GROUP DISTINCT")?;
    let table = args.next().ok_or("missing table")?;
    let group = args.next().ok_or("missing group column")?;
    let distinct = args.next().ok_or("missing distinct column")?;
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let catalog = rudb_native::Catalog::open(&path)?;
    let fields = catalog.table_fields(&table).ok_or("table not found")?;
    let group =
        fields.iter().position(|field| field.name == group).ok_or("group column not found")?;
    let distinct = fields
        .iter()
        .position(|field| field.name == distinct)
        .ok_or("distinct column not found")?;
    if !rudb_native::Writer::certify_grouped_distinct(&path, &table, group, distinct)? {
        return Err("frequency bounds could not certify the top groups".into());
    }
    Ok(())
}
