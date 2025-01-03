use anyhow::{bail, Result};
use clap::{ArgAction, Parser};
use flate2::write::GzEncoder;
use flate2::Compression;
use log::{info, warn};
use polars::prelude::*;
use rayon::prelude::*;
use statrs::distribution::{ChiSquared, ContinuousCDF};
use std::collections::HashMap;
use std::env::set_var;
use std::fs::File;
use std::io::BufRead;

use ldscrs::const_value::{DEFAULT_CNAMES, DESCRIBE_CNAME, NULL_VALUES};
use ldscrs::utils::get_input_reader;

const GROUP: &str = "Column names. NB: case insensitive.";
const TOLERANCE: f64 = 0.1;

#[derive(Parser, Debug)]
#[command(
    name = "munge_sumstats",
    version = "0.1",
    author = "Wenjie Wei <weiwenjie@westlake.edu.cn>",
    about = "Munge summary statistics"
)]
struct Args {
    #[arg(long, default_value = None, help = "Input filename.", required = true)]
    sumstats: String,

    #[arg(long="N", default_value = None, help = "Sample size. If this option is not set, will try to infer the sample size from the input file. If the input file contains a sample size column, and this flag is set, the argument to this flag has priority.")]
    n: Option<f64>,

    #[arg(long="N-cas", default_value = None, help = "Number of cases. If this option is not set, will try to infer the number of cases from the input file. If the input file contains a number of cases column, and this flag is set, the argument to this flag has priority.")]
    n_cas: Option<f64>,

    #[arg(long="N-con", default_value = None, help = "Number of controls. If this option is not set, will try to infer the number of controls from the input file. If the input file contains a number of controls column, and this flag is set, the argument to this flag has priority.")]
    n_con: Option<f64>,

    #[arg(long, default_value = None, help = "Output filename prefix.", required = true)]
    out: String,

    #[arg(long, default_value_t = 0.9, help = "Minimum INFO score.")]
    info_min: f64,

    #[arg(long, default_value_t = 0.01, help = "Minimum MAF.")]
    maf_min: f64,

    #[arg(long, action = ArgAction::SetTrue, help = "Use this flag to parse Stephan Ripke's daner* file format.", conflicts_with = "daner_n")]
    daner: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Use this flag to parse more recent daner* formatted files, which include sample size column 'Nca' and 'Nco'." ,conflicts_with = "daner")]
    daner_n: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Don't require alleles. Useful if only unsigned summary statistics are available and the goal is h2 / partitioned h2 estimation rather than rg estimation.", conflicts_with = "merge_alleles")]
    no_alleles: bool,

    #[arg(long, default_value = None, help = "Same as --merge, except the file should have three columns: SNP, A1, A2, and all alleles will be matched to the --merge-alleles file alleles.", conflicts_with = "no_alleles")]
    merge_alleles: Option<String>,

    #[arg(long, default_value = None, help = "Minimum N (sample size). Default is (90th percentile N) / 2.")]
    n_min: Option<f64>,

    #[arg(long, default_value_t = 5e6 as usize, help = "Chunksize.")]
    chunksize: usize,

    #[arg(long, default_value = None, help = "Name of SNP column (if not a name that ldsc understands). ")]
    snp: Option<String>,

    #[arg(long="N-col", default_value = None, help = "Name of N column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    n_col: Option<String>,

    #[arg(long="N-cas-col", default_value = None, help = "Name of N column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    n_cas_col: Option<String>,

    #[arg(long="N-con-col", default_value = None, help = "Name of N column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    n_con_col: Option<String>,

    #[arg(long, default_value = None, help = "Name of A1 column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    a1: Option<String>,

    #[arg(long, default_value = None, help = "Name of A2 column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    a2: Option<String>,

    #[arg(long, default_value = None, help = "Name of p-value column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    p: Option<String>,

    #[arg(long, default_value = None, help = "Name of FRQ or MAF column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    frq: Option<String>,

    #[arg(long, default_value = None, help = "Name of signed sumstat column, comma null value (e.g., Z,0 or OR,1). ", help_heading=Some(GROUP))]
    signed_sumstats: Option<String>,

    #[arg(long, default_value = None, help = "Name of INFO column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    info: Option<String>,

    #[arg(long, default_value = None, help = "Comma-separated list of INFO columns. Will filter on the mean. ", help_heading=Some(GROUP))]
    info_list: Option<String>,

    #[arg(long, default_value = None, help = "Name of NSTUDY column (if not a name that ldsc understands). ", help_heading=Some(GROUP))]
    nstudy: Option<String>,

    #[arg(long, default_value = None, help = "Minimum # of studies. Default is to remove everything below the max, unless there is an N column, in which case do nothing.", help_heading=Some(GROUP))]
    nstudy_min: Option<f64>,

    #[arg(long, default_value = None, help = "Comma-separated list of column names to ignore.", help_heading=Some(GROUP))]
    ignore: Option<String>,

    #[arg(long, action = ArgAction::SetTrue, help = "A1 is the increasing allele.", help_heading=Some(GROUP))]
    a1_inc: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Keep the MAF column (if one exists).", help_heading=Some(GROUP))]
    keep_maf: bool,
}
fn main() -> Result<()> {
    let args = Args::parse();

    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build_global()?;

    let start = std::time::Instant::now();
    // Initialize logger
    init_logger(&args)?;

    // get colnames
    let colnames = get_file_colnames(&args.sumstats)?;
    info!("Column names: {:?}", colnames);

    // get flag_names and null_value
    let (flag_cnames, signed_sumstst_null) = parse_flag_colnames(&args)?;
    info!("Flag column names: {:?}", flag_cnames);
    info!("Null value: {:?}", signed_sumstst_null);

    // ingore columns
    let ignore_cnames = match &args.ignore {
        Some(ignore) => ignore.split(',').map(clean_header).collect::<Vec<_>>(),
        None => vec![],
    };
    info!("Ignore columns: {:?}", ignore_cnames);

    // remove LOG_ODDS, BETA, Z, OR from the default list
    let mod_default_cnames: HashMap<&str, &str> = DEFAULT_CNAMES
        .into_iter()
        .filter(|&(_, v)| {
            if args.signed_sumstats.is_some() || args.a1_inc {
                !NULL_VALUES.contains_key(v)
            } else {
                true
            }
        })
        .map(|(k, v)| (*k, *v))
        .collect();
    info!("Modified default column names: {:?}", mod_default_cnames);

    // get colnames map
    let cname_map = get_cname_map(flag_cnames, mod_default_cnames, ignore_cnames);
    info!("Column name map: {:?}", cname_map);

    // if daner or daner_n
    // TODO: daner
    if args.daner {
        todo!();
    }
    if args.daner_n {
        todo!();
    }

    let mut cname_translation = colnames
        .iter()
        .filter_map(|x| {
            let clean_x = clean_header(x);
            if cname_map.contains_key(&clean_x) {
                Some((x, cname_map[&clean_x].clone()))
            } else {
                None
            }
        })
        .collect::<HashMap<_, _>>();
    info!("Column name translation: {:?}", cname_translation);

    let cname_description = cname_translation
        .iter()
        .map(|(x, v)| (*x, *DESCRIBE_CNAME.get(v).unwrap_or(&"")))
        .collect::<HashMap<_, _>>();
    info!("Column name description: {:?}", cname_description);

    let (sign_cname, signed_sumstst_null) = if args.signed_sumstats.is_none() && !args.a1_inc {
        let sign_cnames: Vec<_> = cname_translation
            .iter()
            .filter(|(_, v)| NULL_VALUES.contains_key(v))
            .map(|(k, _)| *k)
            .collect();
        match sign_cnames.len() {
            0 => bail!("Could not find a signed summary statistic column."),
            1 => {
                let cname = sign_cnames[0];
                let signed_sumstst_null =
                    Some(*NULL_VALUES.get(cname_translation[&cname].as_str()).unwrap() as f64);
                cname_translation.insert(cname, "SIGNED_SUMSTAT".to_string());
                (cname, signed_sumstst_null)
            }
            _ => bail!(
                "Too many signed sumstat columns. Specify which to ignore with the --ignore flag."
            ),
        }
    } else {
        (&"SIGNED_SUMSTATS".to_string(), signed_sumstst_null)
    };
    info!("Signed column name: {:?}", sign_cname);
    info!("Signed column null value: {:?}", signed_sumstst_null);

    //check that we have all the columns we need
    if !args.a1_inc {
        let req_cols = vec!["SNP", "P", "SIGNED_SUMSTAT"];
        for c in req_cols {
            if !&cname_translation.values().any(|v| v == c) {
                bail!("Could not find {} column.", c);
            }
        }
    } else {
        let req_cols = vec!["SNP", "P"];
        for c in req_cols {
            if !&cname_translation.values().any(|v| v == c) {
                bail!("Could not find {} column.", c);
            }
        }
    }

    //check aren't any duplicate column names in mapping
    for field in cname_translation.keys() {
        let count = colnames.iter().filter(|x| x == field).count();
        if count > 1 {
            bail!("Found {} columns named {}", count, field);
        }
    }

    // check multiple different column names don't map to same data field
    for head in cname_map.values() {
        let count = cname_translation.values().filter(|x| **x == *head).count();
        if count > 1 {
            bail!("Found {} different {} columns", count, head);
        }
    }

    if args.n.is_none()
        && (args.n_cas.is_none() || args.n_con.is_none())
        && !(cname_translation.values().any(|v| v == "N")
            || ["N_CAS", "N_CON"]
                .iter()
                .all(|x| cname_translation.values().any(|v| v == *x)))
    {
        bail!("Could not determine N.");
    }

    if (cname_translation.values().any(|v| v == "N")
        || ["N_CAS", "N_CON"]
            .iter()
            .all(|x| cname_translation.values().any(|v| v == *x)))
        && cname_translation.values().any(|v| v == "NSTUDY")
    {
        let nstudy: Vec<_> = cname_translation
            .iter()
            .filter(|(_, v)| **v == "NSTUDY")
            .map(|(k, _)| *k)
            .collect();
        for x in nstudy {
            cname_translation.remove(&x);
        }
    }

    if !args.no_alleles
        && !["A1", "A2"]
            .iter()
            .all(|x| cname_translation.values().any(|v| v == *x))
    {
        bail!("Could not find A1/A2 columns.");
    }

    info!("Interpreting column names as follows:");
    for (x, desc) in &cname_description {
        info!("{}:\t{}", x, desc);
    }

    let merge_alleles_df = if let Some(ma_path) = &args.merge_alleles {
        Some(get_merge_allels_df(ma_path)?)
    } else {
        None
    };
    info!("Read merge alleles file done.");

    // Start read sumstats
    //  figure out which columns are going to involve sign information, so we can ensure they're read as floats

    let signed_sumstats_cols = cname_translation
        .iter()
        .filter(|(_, v)| *v == "SIGNED_SUMSTAT")
        .map(|(k, _)| *k)
        .collect::<Vec<_>>();
    info!("Signed sumstats columns: {:?}", signed_sumstats_cols);
    let mut sign_schema = Schema::default();
    for c in &signed_sumstats_cols {
        sign_schema.with_column(c.as_str().into(), DataType::Float64);
    }
    // info!("Signed sumstats schema: {:?}", sign_schema);

    // Temporary for reading N, due to some N looks like 7e05 but it's a i64
    sign_schema.with_column("N".into(), DataType::Float64);

    let parse_opts = CsvParseOptions::default()
        .with_separator(b'\t')
        .with_null_values(Some(NullValues::AllColumns(vec![".".into(), "NA".into()])));
    let sumstats_path = args.sumstats.clone();
    let mut sumspd = CsvReadOptions::default()
        .with_parse_options(parse_opts)
        .with_has_header(true)
        .with_columns(Some(
            cname_translation
                .keys()
                .map(|x| x.as_str().into())
                .collect(),
        ))
        // .with_ignore_errors(true)
        .with_schema_overwrite(Some(sign_schema.into()))
        .with_chunk_size(args.chunksize)
        .try_into_reader_with_file_path(Some(sumstats_path.into()))?
        .finish()?;
    // trans N col to i64
    sumspd = sumspd
        .clone()
        .lazy()
        .with_column(col("N").cast(DataType::Int64).alias("N"))
        .collect()?;

    let dat = parse_dat(sumspd, cname_translation, &merge_alleles_df, &args)?;
    let mut dat = process_n(dat, &args)?;
    // trans p to z
    let p_col = dat.column("P")?.f64()?;
    let chi2 = ChiSquared::new(1.0)?;
    // calculate
    let z_values: Vec<f64> = p_col
        .into_no_null_iter()
        .par_bridge()
        .map(|p_val| chi2.inverse_cdf(1.0 - p_val).sqrt())
        .collect();
    let z_series = Series::new("Z".into(), z_values);
    dat.with_column(z_series)?;
    // drop p
    dat.drop_in_place("P")?;

    if !args.a1_inc {
        let median_sign = dat.column("SIGNED_SUMSTAT")?.f64()?.median().unwrap();
        let diff = (median_sign - signed_sumstst_null.unwrap()).abs();
        if diff > TOLERANCE {
            warn!(
                "WARNING: median value of {} is {} (should be close to {}). This column may be mislabeled.",
                sign_cname, median_sign, signed_sumstst_null.unwrap()
            );
        } else {
            info!(
                "Median value of {} was {}, which seems sensible",
                sign_cname, median_sign
            );
        }

        // dat.Z *= (-1) ** (dat.SIGNED_SUMSTAT < signed_sumstat_null)
        let signed_sumstat = dat.column("SIGNED_SUMSTAT")?.f64()?;
        let z = dat.column("Z")?.f64()?;
        let z_values: Vec<f64> = signed_sumstat
            .into_iter()
            .zip(z)
            .map(|(signed, z)| match (signed, z) {
                (Some(signed), Some(z)) if signed < signed_sumstst_null.unwrap() => -z,
                (Some(_), Some(z)) => z,
                _ => f64::NAN,
            })
            .collect();
        dat.with_column(Series::new("Z".into(), z_values))?;
        dat.drop_in_place("SIGNED_SUMSTAT")?;
    }

    if args.merge_alleles.is_some() {
        // compare A1+A2 to MA
        let valid_alleles = Series::new(
            "valid_alleles".into(),
            [
                "GTAC", "ACAC", "ACGT", "GTTG", "CTAG", "CTCT", "ACCA", "CTTC", "AGTC", "GTGT",
                "GTCA", "AGGA", "GACT", "GAGA", "GAAG", "AGCT", "GATC", "CAAC", "CAGT", "TGCA",
                "CACA", "TGAC", "AGAG", "CATG", "TCCT", "TCGA", "TGTG", "TGGT", "CTGA", "TCAG",
                "TCTC", "ACTG",
            ],
        );
        dat = dat
            .clone()
            .lazy()
            .with_column(concat_str([col("A1"), col("A2"), col("MA")], "", false).alias("tmp_MA"))
            .collect()?;
        let origin_len = dat.height();
        dat = dat
            .clone()
            .lazy()
            .filter(col("tmp_MA").is_in(lit(valid_alleles)))
            .collect()?;
        let clean_len = dat.height();
        info!(
            "Removed {} SNPs whose alleles did not match --merge-alleles ({} SNPs remain).",
            origin_len - clean_len,
            clean_len
        );
        dat.drop_in_place("tmp_MA")?;
        dat = dat
            .clone()
            .lazy()
            .join(
                merge_alleles_df.clone().unwrap().lazy(),
                [col("SNP")],
                [col("SNP")],
                JoinArgs::new(JoinType::Right).with_coalesce(JoinCoalesce::CoalesceColumns),
            )
            .collect()?;
    }

    let out_fname = format!("{}.sumstats.gz", args.out);

    let mut print_colnames = dat
        .get_column_names()
        .iter()
        .map(|x| x.as_str())
        // in ['SNP', 'N', 'Z', 'A1', 'A2']
        .filter(|c| ["SNP", "N", "Z", "A1", "A2", "FRQ"].contains(c))
        .collect::<Vec<_>>();
    if !args.keep_maf {
        print_colnames.retain(|x| *x != "FRQ");
    }

    let final_len = dat.height();
    let nomiss_n_mask = dat.column("N")?.i64()?.is_not_null();
    let nomiss_len = dat.column("N")?.i64()?.filter(&nomiss_n_mask)?.len();
    info!(
        "Writing summary statistics for {} SNPs ({} with nonmissing beta) to {}.",
        final_len, nomiss_len, out_fname
    );

    // write to file
    let outfile = File::create(out_fname)?;
    let mut gzip_encoder = GzEncoder::new(outfile, Compression::default());
    CsvWriter::new(&mut gzip_encoder)
        .include_header(true)
        .n_threads(8)
        .with_separator(b'\t')
        .with_null_value("".to_owned())
        .with_float_precision(Some(3))
        .finish(&mut dat.select(print_colnames)?)?;
    gzip_encoder.finish()?;

    let duration = start.elapsed();
    info!("Time elapsed in expensive_function() is: {:?}", duration);
    Ok(())
}

// Figure out which column names to use.
// Priority is
// (1) ignore everything in ignore
// (2) use everything in flags that is not in ignore
// (3) use everything in default that is not in ignore or in flags
// The keys of flag are cleaned. The entries of ignore are not cleaned. The keys of defualt
// are cleaned. But all equality is modulo clean_header().
fn get_cname_map(
    flag: HashMap<String, String>,
    default: HashMap<&str, &str>,
    ignore: Vec<String>,
) -> HashMap<String, String> {
    let clean_ignore = ignore.iter().map(|s| clean_header(s)).collect::<Vec<_>>();
    let mut cname_map = flag
        .into_iter()
        .filter(|(k, _)| !clean_ignore.contains(k))
        .collect::<HashMap<_, _>>();
    default.into_iter().for_each(|(k, v)| {
        if !clean_ignore.contains(&k.to_string()) && !cname_map.contains_key(k) {
            cname_map.insert(k.to_string(), v.to_string());
        }
    });
    cname_map
}

fn init_logger(args: &Args) -> Result<()> {
    let _out_pre = &args.out;
    let _log_file = format!("{}.log", _out_pre);
    // TODO: temp stdout
    set_var("RUST_LOG", "info");
    env_logger::init();
    Ok(())
}

fn get_file_colnames(sumstats_path: &str) -> Result<Vec<String>> {
    // read first line from reader
    let reader = get_input_reader(sumstats_path)?;
    let mut lines = reader.lines();
    if let Some(Ok(line)) = lines.next() {
        let colnames = line.split_whitespace().map(|s| s.to_string()).collect();
        Ok(colnames)
    } else {
        bail!("Empty file: {:?}", sumstats_path);
    }
}

// For cleaning file headers.
//     - convert to uppercase
//     - replace dashes '-' with underscores '_'
//     - replace dots '.' (as in R) with underscores '_'
//     - remove newlines ('\n')
fn clean_header(header: &str) -> String {
    header
        .to_uppercase()
        .trim()
        .replace("-", "_")
        .replace(".", "_")
        .replace("\n", "")
}

/// Parse flags that specify how to interpret nonstandard column names.
/// flag_cnames is a dict that maps (cleaned) arguments to internal column names
fn parse_flag_colnames(args: &Args) -> Result<(HashMap<String, String>, Option<f64>)> {
    let mut flag_cnames: HashMap<String, String> = HashMap::new();
    let cname_options = [
        (&args.nstudy, "NSTUDY"),
        (&args.snp, "SNP"),
        (&args.n_col, "N"),
        (&args.n_cas_col, "N_CAS"),
        (&args.n_con_col, "N_CON"),
        (&args.a1, "A1"),
        (&args.a2, "A2"),
        (&args.p, "P"),
        (&args.frq, "FRQ"),
        (&args.info, "INFO"),
    ];

    for (opt, internal) in &cname_options {
        if let Some(val) = opt {
            flag_cnames.insert(clean_header(val), internal.to_string());
        }
    }

    if let Some(info_list) = &args.info_list {
        match info_list.split(',').map(clean_header).collect::<Vec<_>>() {
            info_headers if !info_headers.is_empty() => {
                for header in info_headers {
                    flag_cnames.insert(header, "INFO".to_string());
                }
            }
            _ => {
                bail!(
                    "The argument to --info-list should be a comma-separated list of column names."
                )
            }
        }
    }

    let mut null_value: Option<f64> = None;
    if let Some(signed_sumstats) = &args.signed_sumstats {
        match signed_sumstats.split(',').collect::<Vec<_>>() {
            parts if parts.len() == 2 => {
                if let Ok(value) = parts[1].parse::<f64>() {
                    null_value = Some(value);
                    flag_cnames.insert(clean_header(parts[0]), "SIGNED_SUMSTAT".to_string());
                } else {
                    bail!(
                        "The argument to --signed-sumstats should be column header comma number."
                    );
                }
            }
            _ => {
                bail!("The argument to --signed-sumstats should be column header comma number.");
            }
        }
    }
    Ok((flag_cnames, null_value))
}

fn get_merge_allels_df(ma_path: &str) -> Result<DataFrame> {
    // merge_alleles = pd.read_csv(args.merge_alleles, compression=compression, header=0,
    //     delim_whitespace=True, na_values='.')
    let parse_opts = CsvParseOptions::default().with_separator(b'\t');
    let mapd = CsvReadOptions::default()
        .with_parse_options(parse_opts)
        .with_has_header(true)
        .try_into_reader_with_file_path(Some(ma_path.into()))?
        .finish()?;

    if !["SNP", "A1", "A2"].iter().all(|x| mapd.column(x).is_ok()) {
        bail!("--merge-alleles must have columns SNP, A1, A2.");
    }
    let ma_len = mapd.height();
    info!("Read {} SNPs for allele merge.", ma_len);

    let mapd = mapd
        .clone()
        .lazy()
        .with_column(concat_str([col("A1"), col("A2")], "", false).alias("MA"))
        .collect()?;

    // drop columns except SNP and MA
    let mapd = mapd.select(&["SNP".to_string(), "MA".to_string()])?;

    Ok(mapd)
}

fn parse_dat(
    dat: DataFrame,
    convert_colname: HashMap<&String, String>,
    merge_alleles: &Option<DataFrame>,
    args: &Args,
) -> Result<DataFrame> {
    let origin_tot_snps = dat.height();
    // let mut dat_list = Vec::new();
    info!("Read {} SNPs from --sumstats file.", origin_tot_snps);
    let mut drops = HashMap::from([
        ("NA", 0),
        ("P", 0),
        ("INFO", 0),
        ("FRQ", 0),
        ("A", 0),
        ("SNP", 0),
        ("MERGE", 0),
    ]);

    // drop NA but keep INFO
    let colnames = dat
        .get_column_names()
        .iter()
        .map(|x| x.as_str().to_string())
        .collect::<Vec<_>>();
    let drop_na_cols = colnames
        .iter()
        .filter(|col| **col != "INFO")
        .map(|cn| cn.into())
        .collect::<Vec<String>>();
    let mut dat = dat.drop_nulls(Some(&drop_na_cols))?;
    let clean_snps = dat.height();
    if let Some(x) = drops.get_mut("NA") {
        *x += origin_tot_snps - clean_snps;
    }
    info!(
        "Removed {} SNPs with missing values.",
        drops.get("NA").unwrap()
    );

    // rename columns
    let new_columns = colnames
        .iter()
        .map(|col| convert_colname.get(col).unwrap().to_string())
        .collect::<Vec<_>>();
    dat.set_column_names(&new_columns)?;

    // join sumstats align with merge_alleles SNP if merge_alleles is not None
    // let mut dat = dat
    //     .clone()
    //     .lazy()
    //     .join(
    //         merge_alleles.clone().lazy(),
    //         [col("SNP")],
    //         [col("SNP")],
    //         JoinArgs::default(),
    //     )
    //     .collect()?;
    let mut dat = match merge_alleles {
        Some(merge_alleles) => dat
            .clone()
            .lazy()
            .join(
                merge_alleles.clone().lazy(),
                [col("SNP")],
                [col("SNP")],
                JoinArgs::default(),
            )
            .collect()?,
        None => dat,
    };

    let merged_count = dat.height();
    if let Some(x) = drops.get_mut("MERGE") {
        *x += clean_snps - merged_count;
    }
    info!(
        "Removed {} SNPs not in --merge-alleles.",
        drops.get("MERGE").unwrap()
    );

    // filter INFO
    if new_columns.contains(&"INFO".to_string()) {
        let bad_info_df = dat
            .clone()
            .lazy()
            // ((info > 2.0) | (info < 0)) & info.notnull
            .filter(
                (col("INFO").gt_eq(2.0).or(col("INFO").lt_eq(0.0))).and(col("INFO").is_not_null()),
            )
            .collect()?;
        let bad_info_count = bad_info_df.height();
        if bad_info_count > 0 {
            warn!(
                "WARNING: {} SNPs had INFO outside of [0,2]. The INFO column may be mislabeled.",
                bad_info_count
            );
        }
        dat = dat
            .clone()
            .lazy()
            .filter(col("INFO").gt_eq(args.info_min))
            .collect()?;

        if let Some(x) = drops.get_mut("INFO") {
            *x += merged_count - dat.height();
        }
    }
    info!(
        "Removed {} SNPs with INFO <= {}.",
        drops.get("INFO").unwrap(),
        args.info_min
    );

    // Filter FRQ
    if new_columns.contains(&"FRQ".to_string()) {
        let bad_frq_df = dat
            .clone()
            .lazy()
            .filter(col("FRQ").lt(0.0).or(col("FRQ").gt(1.0)))
            .collect()?;
        let bad_frq_count = bad_frq_df.height();
        if bad_frq_count > 0 {
            warn!(
                "WARNING: {} SNPs had FRQ outside of [0,1]. The FRQ column may be mislabeled.",
                bad_frq_count
            );
        }
        let low_maf = args.maf_min;
        let high_maf = 1_f64 - args.maf_min;
        let pass_maf_dat = dat
            .clone()
            .lazy()
            .filter(col("FRQ").gt(low_maf).and(col("FRQ").lt_eq(high_maf)))
            .collect()?;
        if let Some(x) = drops.get_mut("FRQ") {
            *x += dat.height() - pass_maf_dat.height();
        }
        dat = pass_maf_dat;
    }
    info!(
        "Removed {} SNPs with MAF <= {}.",
        drops.get("FRQ").unwrap(),
        args.maf_min,
    );

    // drop info and frq if not needed
    if new_columns.contains(&"INFO".to_string()) {
        dat.drop_in_place("INFO")?;
    }
    if new_columns.contains(&"FRQ".to_string()) && !args.keep_maf {
        dat.drop_in_place("FRQ")?;
    }

    // filter P
    let pass_p_df = dat
        .clone()
        .lazy()
        .filter(col("P").gt(0.0).and(col("P").lt_eq(1.0)))
        .collect()?;
    let pass_p_count = pass_p_df.height();
    let bad_p_count = dat.height() - pass_p_count;
    if bad_p_count > 0 {
        warn!(
            "WARNING: {} SNPs had P outside of (0,1]. The P column may be mislabeled.",
            bad_p_count
        );
        if let Some(x) = drops.get_mut("P") {
            *x += bad_p_count;
        }
    }
    dat = pass_p_df;
    info!(
        "Removed {} SNPs with out-of-bounds p-values.",
        drops.get("P").unwrap()
    );

    if !args.no_alleles {
        // A1+A2 in VALID_SNPS
        let valid_snps = Series::new(
            "valid_snps".into(),
            ["AC", "GT", "AG", "CA", "GA", "TG", "TC", "CT"],
        );
        let mut pass_alleles_df = dat
            .clone()
            .lazy()
            .with_column(concat_str([col("A1"), col("A2")], "", false).alias("tmp_MA"))
            .filter(col("tmp_MA").is_in(lit(valid_snps)))
            .collect()?;
        // drop tmp_MA
        pass_alleles_df.drop_in_place("tmp_MA")?;
        let pass_alleles_count = pass_alleles_df.height();
        if let Some(x) = drops.get_mut("A") {
            *x += dat.height() - pass_alleles_count;
        }
        dat = pass_alleles_df;
    }
    info!(
        "Removed {} variants that were not SNPs or were strand-ambiguous.",
        drops.get("A").unwrap()
    );

    let remain_count = dat.height();
    if remain_count == 0 {
        bail!("After applying filters, no SNPs remain.");
    }
    info!("{} SNPs remained", remain_count);
    info!("Done.");

    // remove dup SNPs
    // unique SNP
    let unique_dat = dat
        .clone()
        .lazy()
        .unique_stable(Some(vec!["SNP".into()]), UniqueKeepStrategy::Any)
        .collect()?;
    let dup_count = dat.height() - unique_dat.height();
    dat = unique_dat;
    info!(
        "Removed {} SNPs with duplicated rs numbers ({} SNPs remain).",
        dup_count,
        dat.height()
    );

    Ok(dat)
}

// Determine sample size from --N* flags or N* columns. Filter out low N SNPs.s
fn process_n(dat: DataFrame, args: &Args) -> Result<DataFrame> {
    let colnames = dat
        .get_column_names()
        .iter()
        .map(|x| x.as_str())
        .collect::<Vec<_>>();
    let mut dat = dat.clone();
    if colnames.contains(&"N_CAS") && colnames.contains(&"N_CON") {
        let n_cas = dat.column("N_CAS")?.i64()?;
        let n_con = dat.column("N_CON")?.i64()?;
        let n = n_cas + n_con;
        let p = (&n_cas.cast(&DataType::Float64)? / &n.cast(&DataType::Float64)?)?;
        let max_n = n.max().unwrap();
        let p_max_n = p.filter(&n.equal(max_n))?.mean().unwrap();
        let new_n_series = Series::new("N".into(), n_cas.cast(&DataType::Float64)? / p_max_n);
        dat.with_column(new_n_series)?;
        dat.drop_in_place("N_CAS")?;
        dat.drop_in_place("N_CON")?;
    }

    if colnames.contains(&"N") {
        let n_min = if let Some(n_min) = args.n_min {
            n_min
        } else {
            let n = dat.column("N")?.i64()?;
            n.quantile(0.9, QuantileMethod::Linear)?.unwrap() / 1.5
        };
        let old_count = dat.height();
        dat = dat.lazy().filter(col("N").gt_eq(lit(n_min))).collect()?;
        let new_count = dat.height();
        info!(
            "Removed {} SNPs with N < {} ({} SNPs remain).",
            old_count - new_count,
            n_min,
            new_count
        );
    } else if colnames.contains(&"NSTUDY") && !colnames.contains(&"N") {
        let nstudy_min = if let Some(nstudy_min) = args.nstudy_min {
            nstudy_min
        } else {
            let nstudy = dat.column("NSTUDY")?.f64()?;
            nstudy.max().unwrap()
        };
        let old_count = dat.height();
        dat = dat
            .lazy()
            .filter(col("NSTUDY").gt_eq(lit(nstudy_min)))
            .collect()?;
        dat.drop_in_place("NSTUDY")?;
        let new_count = dat.height();
        info!(
            "Removed {} SNPs with NSTUDY < {} ({} SNPs remain).",
            old_count - new_count,
            nstudy_min,
            new_count
        );
    }

    if !colnames.contains(&"N") {
        if let Some(n) = args.n {
            dat = dat.lazy().with_column(lit(n).alias("N")).collect()?;
            info!("Using N = {}", n);
        } else if let (Some(n_cas), Some(n_con)) = (args.n_cas, args.n_con) {
            let n = n_cas + n_con;
            dat = dat.lazy().with_column(lit(n).alias("N")).collect()?;
            if !args.daner {
                info!("Using N_cas = {}; N_con = {}", n_cas, n_con);
            }
        } else {
            bail!(
                "Cannot determine N. This message indicates a bug.\nN should have been checked earlier in the program."
            );
        }
    }
    Ok(dat)
}
