fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path =
        args.next().ok_or("usage: certify_grouped_metrics FILE TABLE GROUP SUM AVG DISTINCT")?;
    let table = args.next().ok_or("missing table")?;
    let names = [
        args.next().ok_or("missing group column")?,
        args.next().ok_or("missing sum column")?,
        args.next().ok_or("missing average column")?,
        args.next().ok_or("missing distinct column")?,
    ];
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let catalog = rudb_native::Catalog::open(&path)?;
    let fields = catalog.table_fields(&table).ok_or("table not found")?;
    let columns = names.map(|name| fields.iter().position(|field| field.name == name));
    let [Some(group), Some(sum), Some(average), Some(distinct)] = columns else {
        return Err("one of the selected columns was not found".into());
    };
    if !rudb_native::Writer::certify_grouped_metrics(
        &path,
        &table,
        [group, sum, average, distinct],
    )? {
        return Err("frequency bounds could not certify the top groups".into());
    }
    Ok(())
}
