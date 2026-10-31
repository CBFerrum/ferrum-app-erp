//! Italy: on the EU's VAT (22%, 10%, 5%, 4% from TEDB), every invoice between businesses is a
//! FatturaPA (FPR12, schema 1.2.3 from 1 April 2025) cleared through the Sistema di Interscambio.
//! A submitted invoice's FatturaPA is written from the EU's EN 16931 model and the Italian data
//! (codice fiscale, regime fiscale, REA, the customer's codice destinatario or PEC), checked
//! against SDI's main rules, and queued for the company's SDI channel (an accredited intermediary
//! or its own SDICoop or SFTP link) through the outbox; the channel records SDI's receipts. Lines
//! without VAT carry their Natura (N1 to N7: exports N3.1, intra-Community supplies N3.2, services
//! to businesses abroad N2.1, exempt N4, the domestic reverse charge N6.x); invoices with more
//! than €77.47 not subject to VAT carry the €2 virtual stamp duty when the company pays it. Small
//! businesses under the regime forfettario charge no VAT (RF19, N2.2); the domestic reverse charge
//! (subcontracting in construction and the like) is the item's.

use crate::member::{Member, Rules};
use crate::util::{d, get, id, rows, s, sys};
use async_trait::async_trait;
use ferrum_eu::ubl::invoice;
use ferrum_eu::vat::{INPUT_TAX, OSS_VAT, OUTPUT_TAX};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result, outbox};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;

pub const SEND_TOPIC: &str = "it.sdi.send_requested";
pub const NAMESPACE: &str = "http://ivaservizi.agenziaentrate.gov.it/docs/xsd/fatture/v1.2";

pub static MEMBER: Member = Member {
    package: "it",
    country: "IT",
    small: Some((
        "it_forfettario",
        "Regime forfettario (art. 1, commi 54-89, L. 190/2014)",
        "Operazione effettuata ai sensi dell'articolo 1, commi da 54 a 89, della Legge n. 190/2014: operazione senza applicazione dell'IVA.",
    )),
    reverse: Some(("it_reverse_charge", "Inversione contabile: IVA a carico del committente (art. 17, comma 6, D.P.R. 633/1972).")),
};

/// A partita IVA: eleven digits, the last checking the rest (Luhn's way).
pub fn partita_iva(raw: &str) -> Option<String> {
    let n: String = raw.trim().trim_start_matches(['I', 'T', 'i', 't']).chars().filter(|c| !c.is_whitespace()).collect();
    if n.len() != 11 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d: Vec<u32> = n.bytes().map(|c| u32::from(c - b'0')).collect();
    let sum: u32 = (0..10)
        .map(|i| {
            if i % 2 == 0 {
                d[i]
            } else {
                let x = d[i] * 2;
                if x > 9 { x - 9 } else { x }
            }
        })
        .sum();
    ((10 - sum % 10) % 10 == d[10]).then_some(n)
}

/// A codice fiscale: a person's 16 characters, or a company's 11 digits (its partita IVA's form).
pub fn codice_fiscale(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();
    match n.len() {
        11 => partita_iva(&n),
        16 if n.chars().all(|c| c.is_ascii_alphanumeric()) => Some(n),
        _ => None,
    }
}

/// The Natura of a line without VAT, from its EN 16931 category, the reason it carries, whether
/// the buyer is abroad, and the item's own code.
pub fn natura(category: &str, reason: Option<&str>, abroad: bool, own: Option<&str>) -> Option<&'static str> {
    let own = own.and_then(|o| NATURE.iter().find(|(c, _)| *c == o).map(|(c, _)| *c));
    match category {
        "S" => None,
        "E" if reason.is_some_and(|r| r.contains("forfettario")) => Some("N2.2"),
        "E" => own.or(Some("N4")),
        "AE" if abroad => Some("N2.1"),
        "AE" => own.filter(|o| o.starts_with("N6")).or(Some("N6.9")),
        "K" => Some("N3.2"),
        "G" => Some("N3.1"),
        "O" if abroad => Some("N2.1"),
        "O" => own.or(Some("N2.2")),
        _ => own.or(Some("N3.6")),
    }
}

/// The Natura codes and the law each one's lines cite.
pub const NATURE: &[(&str, &str)] = &[
    ("N1", "Escluse ex art. 15 D.P.R. 633/1972"),
    ("N2.1", "Non soggette ad IVA ai sensi degli artt. da 7 a 7-septies D.P.R. 633/1972"),
    ("N2.2", "Non soggette - altri casi"),
    ("N3.1", "Non imponibili - esportazioni (art. 8 D.P.R. 633/1972)"),
    ("N3.2", "Non imponibili - cessioni intracomunitarie (art. 41 D.L. 331/1993)"),
    ("N3.3", "Non imponibili - cessioni verso San Marino"),
    ("N3.4", "Non imponibili - operazioni assimilate alle cessioni all'esportazione"),
    ("N3.5", "Non imponibili - a seguito di dichiarazioni d'intento"),
    ("N3.6", "Non imponibili - altre operazioni"),
    ("N4", "Esenti (art. 10 D.P.R. 633/1972)"),
    ("N5", "Regime del margine / IVA non esposta in fattura"),
    ("N6.1", "Inversione contabile - cessione di rottami e altri materiali di recupero"),
    ("N6.2", "Inversione contabile - cessione di oro e argento"),
    ("N6.3", "Inversione contabile - subappalto nel settore edile"),
    ("N6.4", "Inversione contabile - cessione di fabbricati"),
    ("N6.5", "Inversione contabile - cessione di telefoni cellulari"),
    ("N6.6", "Inversione contabile - cessione di prodotti elettronici"),
    ("N6.7", "Inversione contabile - prestazioni comparto edile e settori connessi"),
    ("N6.8", "Inversione contabile - operazioni settore energetico"),
    ("N6.9", "Inversione contabile - altri casi"),
    ("N7", "IVA assolta in altro stato UE"),
];

fn esc(v: &str) -> String {
    v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn r(v: Decimal, dp: u32) -> String {
    let mut x = v.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(dp);
    x.to_string()
}

/// A party's address as FatturaPA's Sede: (street, CAP, town, province, country).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sede {
    pub street: String,
    pub cap: String,
    pub town: String,
    pub province: Option<String>,
    pub country: String,
}

#[derive(Debug, Clone, Default)]
pub struct Party {
    pub name: String,
    /// IdFiscaleIVA: (country, code).
    pub vat: Option<(String, String)>,
    pub codice_fiscale: Option<String>,
    pub sede: Sede,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub description: String,
    pub quantity: Decimal,
    pub unit: Option<String>,
    pub total: Decimal,
    pub rate: Decimal,
    pub natura: Option<&'static str>,
}

/// A FatturaPA's content.
#[derive(Debug, Clone)]
pub struct Fattura {
    pub transmitter: (String, String),
    pub progressive: String,
    pub recipient_code: String,
    pub pec: Option<String>,
    pub seller: Party,
    pub regime: String,
    /// IscrizioneREA: (office, number, in liquidation).
    pub rea: Option<(String, String, bool)>,
    pub buyer: Party,
    pub credit_note: bool,
    pub currency: String,
    pub date: String,
    pub number: String,
    pub stamp_duty: bool,
    pub total: Decimal,
    pub original: Option<(String, String)>,
    pub lines: Vec<Line>,
    /// VAT by rate (of standard-rated lines).
    pub tax: BTreeMap<String, Decimal>,
    pub due: Option<String>,
    pub iban: Option<String>,
    pub notes: Vec<String>,
}

impl Fattura {
    /// DatiRiepilogo: (rate, natura) → (taxable, tax).
    pub fn summary(&self) -> BTreeMap<(String, Option<&'static str>), (Decimal, Decimal)> {
        let mut out: BTreeMap<(String, Option<&'static str>), (Decimal, Decimal)> = BTreeMap::new();
        for l in &self.lines {
            out.entry((r(l.rate, 2), l.natura)).or_default().0 += l.total;
        }
        for ((rate, natura), v) in out.iter_mut() {
            if natura.is_none() {
                v.1 = self.tax.get(rate).copied().unwrap_or_default();
            }
        }
        out
    }

    /// The amount of the lines not subject to VAT that bear the stamp duty.
    pub fn stamped(&self) -> Decimal {
        self.lines.iter().filter(|l| l.natura.is_some_and(|n| !matches!(n, "N3.1" | "N3.2") && !n.starts_with("N6") && n != "N7")).map(|l| l.total).sum()
    }

    /// The file's name: IT, the transmitter's code, and the progressive.
    pub fn file_name(&self) -> String {
        format!("{}{}_{}.xml", self.transmitter.0, self.transmitter.1, self.progressive)
    }
}

/// SDI's checks a FatturaPA written here could fail: the problems, empty when it passes.
pub fn validate(f: &Fattura) -> Vec<String> {
    let mut p = Vec::new();
    let mut rule = |ok: bool, what: &str| {
        if !ok {
            p.push(what.to_string());
        }
    };
    rule(
        f.recipient_code.len() == 7 && f.recipient_code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "the customer's codice destinatario has 7 letters or digits",
    );
    rule(f.seller.vat.is_some(), "the company's partita IVA (its EU VAT number)");
    rule(!f.seller.sede.street.is_empty() && f.seller.sede.cap.len() == 5 && !f.seller.sede.town.is_empty(), "the company's address: street, CAP and comune");
    rule(f.seller.sede.country != "IT" || f.seller.sede.province.as_ref().is_some_and(|p| p.len() == 2), "the company's provincia (two letters)");
    rule(f.buyer.vat.is_some() || f.buyer.codice_fiscale.is_some(), "the customer's partita IVA or codice fiscale (00305: one of them)");
    rule(
        !f.buyer.sede.street.is_empty() && f.buyer.sede.cap.len() == 5 && !f.buyer.sede.town.is_empty(),
        "the customer's billing address: street, CAP (00000 abroad) and town",
    );
    rule(!f.lines.is_empty(), "the invoice has lines");
    rule(f.lines.iter().all(|l| l.rate > Decimal::ZERO || l.natura.is_some()), "every line without VAT has its Natura (00400)");
    rule(f.lines.iter().all(|l| l.rate.is_zero() || l.natura.is_none()), "a line with VAT has no Natura (00401)");
    for ((rate, natura), (taxable, tax)) in f.summary() {
        if natura.is_none() {
            let expected = taxable * Decimal::from_str_exact(&rate).unwrap_or_default() / Decimal::ONE_HUNDRED;
            rule((expected - tax).abs() <= Decimal::ONE, &format!("the VAT at {rate}% is the rate on its taxable amount (00421)"));
        }
    }
    p
}

/// The FatturaPA's XML (FPR12).
pub fn to_xml(f: &Fattura) -> String {
    let mut x = String::new();
    let _ = write!(
        x,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<p:FatturaElettronica versione=\"FPR12\" xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" xmlns:p=\"{NAMESPACE}\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">"
    );
    x.push_str("<FatturaElettronicaHeader><DatiTrasmissione>");
    let _ = write!(
        x,
        "<IdTrasmittente><IdPaese>{}</IdPaese><IdCodice>{}</IdCodice></IdTrasmittente><ProgressivoInvio>{}</ProgressivoInvio><FormatoTrasmissione>FPR12</FormatoTrasmissione><CodiceDestinatario>{}</CodiceDestinatario>",
        esc(&f.transmitter.0),
        esc(&f.transmitter.1),
        esc(&f.progressive),
        esc(&f.recipient_code)
    );
    if let Some(pec) = f.pec.as_ref().filter(|_| f.recipient_code == "0000000") {
        let _ = write!(x, "<PECDestinatario>{}</PECDestinatario>", esc(pec));
    }
    x.push_str("</DatiTrasmissione>");
    let party = |x: &mut String, tag: &str, p: &Party, extra: &str| {
        let _ = write!(x, "<{tag}><DatiAnagrafici>");
        if let Some((c, code)) = &p.vat {
            let _ = write!(x, "<IdFiscaleIVA><IdPaese>{}</IdPaese><IdCodice>{}</IdCodice></IdFiscaleIVA>", esc(c), esc(code));
        }
        if let Some(cf) = &p.codice_fiscale {
            let _ = write!(x, "<CodiceFiscale>{}</CodiceFiscale>", esc(cf));
        }
        let _ = write!(x, "<Anagrafica><Denominazione>{}</Denominazione></Anagrafica>{extra}</DatiAnagrafici>", esc(&p.name));
        let _ = write!(x, "<Sede><Indirizzo>{}</Indirizzo><CAP>{}</CAP><Comune>{}</Comune>", esc(&p.sede.street), esc(&p.sede.cap), esc(&p.sede.town));
        if let Some(pr) = &p.sede.province {
            let _ = write!(x, "<Provincia>{}</Provincia>", esc(pr));
        }
        let _ = write!(x, "<Nazione>{}</Nazione></Sede>", esc(&p.sede.country));
    };
    party(&mut x, "CedentePrestatore", &f.seller, &format!("<RegimeFiscale>{}</RegimeFiscale>", esc(&f.regime)));
    if let Some((office, number, liquidation)) = &f.rea {
        let _ = write!(
            x,
            "<IscrizioneREA><Ufficio>{}</Ufficio><NumeroREA>{}</NumeroREA><StatoLiquidazione>{}</StatoLiquidazione></IscrizioneREA>",
            esc(office),
            esc(number),
            if *liquidation { "LS" } else { "LN" }
        );
    }
    x.push_str("</CedentePrestatore>");
    party(&mut x, "CessionarioCommittente", &f.buyer, "");
    x.push_str("</CessionarioCommittente></FatturaElettronicaHeader><FatturaElettronicaBody><DatiGenerali><DatiGeneraliDocumento>");
    let _ = write!(
        x,
        "<TipoDocumento>{}</TipoDocumento><Divisa>{}</Divisa><Data>{}</Data><Numero>{}</Numero>",
        if f.credit_note { "TD04" } else { "TD01" },
        esc(&f.currency),
        esc(&f.date),
        esc(&f.number)
    );
    if f.stamp_duty {
        x.push_str("<DatiBollo><BolloVirtuale>SI</BolloVirtuale><ImportoBollo>2.00</ImportoBollo></DatiBollo>");
    }
    let _ = write!(x, "<ImportoTotaleDocumento>{}</ImportoTotaleDocumento>", r(f.total, 2));
    for n in &f.notes {
        for chunk in n.chars().collect::<Vec<_>>().chunks(200) {
            let _ = write!(x, "<Causale>{}</Causale>", esc(&chunk.iter().collect::<String>()));
        }
    }
    x.push_str("</DatiGeneraliDocumento>");
    if let Some((number, date)) = &f.original {
        let _ = write!(x, "<DatiFattureCollegate><IdDocumento>{}</IdDocumento><Data>{}</Data></DatiFattureCollegate>", esc(number), esc(date));
    }
    x.push_str("</DatiGenerali><DatiBeniServizi>");
    for (i, l) in f.lines.iter().enumerate() {
        let qty = if l.quantity.is_zero() { Decimal::ONE } else { l.quantity };
        let _ = write!(
            x,
            "<DettaglioLinee><NumeroLinea>{}</NumeroLinea><Descrizione>{}</Descrizione><Quantita>{}</Quantita>",
            i + 1,
            esc(&l.description),
            r(qty, 2)
        );
        if let Some(u) = &l.unit {
            let _ = write!(x, "<UnitaMisura>{}</UnitaMisura>", esc(&u.chars().take(10).collect::<String>()));
        }
        let _ = write!(
            x,
            "<PrezzoUnitario>{}</PrezzoUnitario><PrezzoTotale>{}</PrezzoTotale><AliquotaIVA>{}</AliquotaIVA>",
            r(l.total / qty, 8),
            r(l.total, 2),
            r(l.rate, 2)
        );
        if let Some(n) = l.natura {
            let _ = write!(x, "<Natura>{n}</Natura>");
        }
        x.push_str("</DettaglioLinee>");
    }
    for ((rate, natura), (taxable, tax)) in f.summary() {
        let _ = write!(x, "<DatiRiepilogo><AliquotaIVA>{rate}</AliquotaIVA>");
        if let Some(n) = natura {
            let _ = write!(x, "<Natura>{n}</Natura>");
        }
        let _ = write!(x, "<ImponibileImporto>{}</ImponibileImporto><Imposta>{}</Imposta>", r(taxable, 2), r(tax, 2));
        match natura {
            None => x.push_str("<EsigibilitaIVA>I</EsigibilitaIVA>"),
            Some(n) => {
                let law = NATURE.iter().find(|(c, _)| *c == n).map(|(_, l)| *l).unwrap_or_default();
                let _ = write!(x, "<RiferimentoNormativo>{}</RiferimentoNormativo>", esc(&law.chars().take(100).collect::<String>()));
            }
        }
        x.push_str("</DatiRiepilogo>");
    }
    x.push_str("</DatiBeniServizi>");
    if let Some(iban) = &f.iban {
        x.push_str("<DatiPagamento><CondizioniPagamento>TP02</CondizioniPagamento><DettaglioPagamento><ModalitaPagamento>MP05</ModalitaPagamento>");
        if let Some(due) = &f.due {
            let _ = write!(x, "<DataScadenzaPagamento>{}</DataScadenzaPagamento>", esc(due));
        }
        let _ = write!(x, "<ImportoPagamento>{}</ImportoPagamento><IBAN>{}</IBAN></DettaglioPagamento></DatiPagamento>", r(f.total, 2), esc(iban));
    }
    x.push_str("</FatturaElettronicaBody></p:FatturaElettronica>");
    x
}

fn sede(a: Option<&Doc>) -> Sede {
    let Some(a) = a else { return Sede::default() };
    let country = s(a, "country_code").unwrap_or("IT").to_string();
    let italian = country == "IT";
    Sede {
        street: [s(a, "address_line1"), s(a, "address_line2")].into_iter().flatten().collect::<Vec<_>>().join(", "),
        cap: if italian { s(a, "postal_code").unwrap_or_default().trim().to_string() } else { "00000".into() },
        town: s(a, "city").unwrap_or_default().to_string(),
        province: if italian { s(a, "state").map(|p| p.trim().to_uppercase()).filter(|p| p.len() == 2) } else { None },
        country,
    }
}

fn vat_id(vat: Option<&str>) -> Option<(String, String)> {
    let v: String = vat?.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    (v.len() > 2 && v[..2].chars().all(|c| c.is_ascii_alphabetic())).then(|| {
        let country = if &v[..2] == "EL" { "GR".to_string() } else { v[..2].to_string() };
        (country, v[2..].to_string())
    })
}

/// A submitted sales invoice's FatturaPA (`progressive`: the transmission's, five characters).
pub async fn fattura(hc: &mut HookCtx<'_>, doc: &Doc, progressive: &str) -> Result<Fattura> {
    let inv = invoice(hc, doc).await?;
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
    let cid = id(&company, "id").unwrap_or_default();
    let mut vat_accounts = Vec::new();
    for n in [OUTPUT_TAX, INPUT_TAX, OSS_VAT] {
        let a = crate::util::find(hc.kernel, &ctx, hc.tx, "Account", vec![Filter::eq("company", cid.to_string()), Filter::eq("account_number", n)], None, 1)
            .await?;
        vat_accounts.extend(a.first().and_then(|a| s(a, "id")).map(str::to_string));
    }
    let seller_vat =
        vat_id(s(&company, "eu_vat_number")).ok_or_else(|| Error::invalid("set the company's partita IVA (its EU VAT number, IT and 11 digits)"))?;
    let address = |f: &str| id(doc, f);
    let buyer_address = match address("customer_address") {
        Some(a) => crate::util::find(hc.kernel, &ctx, hc.tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.into_iter().next(),
        None => None,
    };
    let buyer_sede = sede(buyer_address.as_ref());
    let abroad = buyer_sede.country != "IT";
    let sign = if inv.credit_note { -Decimal::ONE } else { Decimal::ONE };
    let mut lines = Vec::new();
    let mut tax: BTreeMap<String, Decimal> = BTreeMap::new();
    for row in rows(doc, "items") {
        let account = vat_accounts.iter().find(|a| row.get("item_tax_rates").and_then(|m| m.get(a.as_str())).is_some()).cloned();
        let rate: Decimal = account.as_ref().and_then(|a| row["item_tax_rates"][a.as_str()].as_str()).and_then(|v| v.parse().ok()).unwrap_or_default();
        let cat = account.as_ref().map(|a| &row["tax_categories"][a.as_str()]).cloned().unwrap_or(Value::Null);
        let code = ferrum_eu::ubl::category_code(cat["category"].as_str(), rate);
        let own = match id(&row, "item") {
            Some(i) => s(&get(hc.kernel, &ctx, hc.tx, "Item", i).await?, "it_natura").map(str::to_string),
            None => None,
        };
        let natura = natura(code, cat["exemption_reason"].as_str(), abroad, own.as_deref());
        let unit = match id(&row, "uom") {
            Some(u) => s(&get(hc.kernel, &ctx, hc.tx, "Uom", u).await?, "uom_name").map(str::to_string),
            None => None,
        };
        lines.push(Line {
            description: s(&row, "item_name").unwrap_or("-").chars().take(1000).collect(),
            quantity: (sign * d(&row, "qty")).abs(),
            unit,
            total: sign * d(&row, "net_amount"),
            rate: if natura.is_some() { Decimal::ZERO } else { rate },
            natura,
        });
    }
    for t in inv.subtotals.iter().filter(|t| t.category == "S") {
        *tax.entry(r(t.percent, 2)).or_default() += t.tax;
    }
    let seller = Party {
        name: inv.seller.name.clone(),
        vat: Some(seller_vat.clone()),
        codice_fiscale: s(&company, "it_codice_fiscale").map(str::to_string),
        sede: Sede {
            street: inv.seller.street.clone().unwrap_or_default(),
            cap: inv.seller.postal.clone().unwrap_or_default(),
            town: inv.seller.city.clone().unwrap_or_default(),
            province: s(&company, "it_provincia").map(|p| p.trim().to_uppercase()),
            country: "IT".into(),
        },
    };
    let buyer_vat = vat_id(s(doc, "eu_customer_vat").or(s(&customer, "eu_vat_number")));
    let buyer = Party {
        name: inv.buyer.name.clone(),
        codice_fiscale: s(&customer, "it_codice_fiscale").map(str::to_string),
        vat: buyer_vat.or_else(|| abroad.then(|| (buyer_sede.country.clone(), "99999999999".to_string()))),
        sede: buyer_sede,
    };
    let recipient_code = match s(&customer, "it_codice_destinatario").map(|c| c.trim().to_uppercase()) {
        Some(c) => c,
        None if abroad => "XXXXXXX".into(),
        None => "0000000".into(),
    };
    let rea = match (s(&company, "it_rea_ufficio"), s(&company, "it_rea_numero")) {
        (Some(o), Some(n)) => Some((o.to_uppercase(), n.to_string(), crate::util::b(&company, "it_in_liquidazione"))),
        _ => None,
    };
    let regime = if crate::util::b(&company, "it_forfettario") { "RF19".to_string() } else { s(&company, "it_regime_fiscale").unwrap_or("RF01").to_string() };
    let mut f = Fattura {
        transmitter: seller_vat.clone(),
        progressive: progressive.to_string(),
        recipient_code,
        pec: s(&customer, "it_pec").map(str::to_string),
        seller,
        regime,
        rea,
        buyer,
        credit_note: inv.credit_note,
        currency: inv.currency.clone(),
        date: inv.issue_date.clone(),
        number: inv.number.clone(),
        stamp_duty: false,
        total: inv.tax_inclusive,
        original: inv.original.clone(),
        lines,
        tax,
        due: inv.due_date.clone(),
        iban: inv.payment.as_ref().and_then(|p| p.iban.clone()),
        notes: inv.notes.clone(),
    };
    f.stamp_duty = crate::util::b(&company, "it_bollo_virtuale") && f.stamped() > Decimal::new(7747, 2);
    let problems = validate(&f);
    if !problems.is_empty() {
        return Err(Error::invalid(format!("the FatturaPA would be refused: {}", problems.join("; "))));
    }
    Ok(f)
}

/// The next transmission's progressive for the company: five characters, base 36.
async fn next_progressive(hc: &mut HookCtx<'_>, company: uuid::Uuid) -> Result<String> {
    let ctx = sys(hc.ctx);
    let co = hc.kernel.get_tx(&ctx, hc.tx, "Company", company, true).await?;
    let n = co.get("it_sdi_counter").and_then(Value::as_i64).unwrap_or(0) + 1;
    let Value::Object(patch) = json!({"it_sdi_counter": n}) else { return Err(Error::internal("counter")) };
    hc.kernel.set_values_tx(&ctx, hc.tx, "Company", company, patch).await?;
    let digits = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut v = n as u64 % 36u64.pow(5);
    let mut out = [b'0'; 5];
    for slot in out.iter_mut().rev() {
        *slot = digits[(v % 36) as usize];
        v /= 36;
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

/// FatturaPA on submitted invoices: download, send to SDI (through the outbox), record SDI's receipts.
pub struct SdiHook;

impl SdiHook {
    async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }
}

/// SDI's receipts and what each makes of the invoice.
pub const RECEIPTS: &[(&str, &str)] =
    &[("RC", "Delivered"), ("NS", "Rejected"), ("MC", "Not Delivered"), ("AT", "Not Delivered"), ("DT", "Delivered"), ("NE", "Delivered"), ("EC", "Delivered")];

#[async_trait]
impl DocHook for SdiHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        for field in ["it_codice_fiscale", "it_codice_destinatario"] {
            if hc.entity.field(field).is_none() {
                continue;
            }
            let Some(v) = s(doc, field).map(|v| v.trim().to_uppercase()).filter(|v| !v.is_empty()) else { continue };
            let ok = match field {
                "it_codice_fiscale" => codice_fiscale(&v),
                _ => (v.len() == 7 && v.chars().all(|c| c.is_ascii_alphanumeric())).then(|| v.clone()),
            };
            match ok {
                Some(n) => crate::util::set(doc, field, n),
                None => return Err(Error::Validation(vec![FieldError::new(field, "is not valid")])),
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("it_sdi_status").is_none() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        if s(&company, "country_code") == Some("IT") && crate::util::b(&company, "it_sdi_on_submit") {
            send(hc, doc, cid).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "it_sdi_status"), Some("Pending" | "Sent" | "Delivered")) {
            return Err(Error::invalid("SDI has this invoice: issue a credit note (TD04) instead of cancelling it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if hc.entity.field("it_sdi_status").is_none() || !name.starts_with("it_") {
            return Ok(None);
        }
        let cid = id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?;
        match name {
            "it_fatturapa" => {
                let progressive =
                    s(doc, "it_sdi_file").and_then(|f| f.rsplit('_').next()).map(|p| p.trim_end_matches(".xml").to_string()).unwrap_or_else(|| "00000".into());
                let f = fattura(hc, doc, &progressive).await?;
                Ok(Some(json!({"file": {"name": f.file_name(), "type": "application/xml", "content": to_xml(&f)}})))
            }
            "it_send_sdi" => send(hc, doc, cid).await.map(Some),
            "it_record_sdi" => {
                let kind = params
                    .get("receipt")
                    .and_then(Value::as_str)
                    .map(str::to_uppercase)
                    .ok_or_else(|| Error::field("receipt", "is required (RC, NS, MC...)"))?;
                let status =
                    RECEIPTS.iter().find(|(k, _)| *k == kind).map(|(_, s)| *s).ok_or_else(|| Error::field("receipt", "is not one of SDI's receipts"))?;
                let sdi_id = params.get("sdi_id").and_then(Value::as_str).map(str::to_string);
                let message = params.get("message").and_then(Value::as_str).map(|m| m.chars().take(2000).collect::<String>());
                Self::save(hc, doc, json!({"it_sdi_status": status, "it_sdi_id": sdi_id.or(s(doc, "it_sdi_id").map(str::to_string)), "it_sdi_error": if status == "Delivered" { None } else { message }})).await?;
                Ok(Some(json!({"status": status})))
            }
            _ => Ok(None),
        }
    }
}

/// Writes the invoice's FatturaPA with the next progressive and queues it for SDI.
async fn send(hc: &mut HookCtx<'_>, doc: &Doc, company: uuid::Uuid) -> Result<Value> {
    if matches!(s(doc, "it_sdi_status"), Some("Pending" | "Sent" | "Delivered")) {
        return Err(Error::invalid(format!("the invoice is already {}", s(doc, "it_sdi_status").unwrap_or_default().to_lowercase())));
    }
    let progressive = next_progressive(hc, company).await?;
    let f = fattura(hc, doc, &progressive).await?;
    let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let event = json!({"invoice": iid, "company": company, "number": f.number, "file_name": f.file_name(), "xml": to_xml(&f)});
    SdiHook::save(hc, doc, json!({"it_sdi_status": "Pending", "it_sdi_file": f.file_name(), "it_sdi_error": null})).await?;
    outbox::enqueue(hc.tx, hc.ctx.tenant, SEND_TOPIC, Some("SalesInvoice"), Some(iid), event).await?;
    Ok(json!({"status": "Pending", "file": f.file_name()}))
}

pub fn rules() -> Rules {
    Rules(&MEMBER)
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "SalesInvoice"] {
        h.register(entity, std::sync::Arc::new(SdiHook));
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("it");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_natura() {
        assert_eq!(partita_iva("IT 01234567897").as_deref(), Some("01234567897"));
        assert_eq!(partita_iva("01234567890"), None);
        assert_eq!(codice_fiscale("rssmra85t10a562s").as_deref(), Some("RSSMRA85T10A562S"));
        assert_eq!(natura("G", None, true, None), Some("N3.1"));
        assert_eq!(natura("K", None, true, None), Some("N3.2"));
        assert_eq!(natura("AE", Some("VATEX-EU-AE"), true, None), Some("N2.1"), "services to a business elsewhere in the EU");
        assert_eq!(natura("AE", Some("VATEX-EU-AE"), false, Some("N6.3")), Some("N6.3"), "subcontracting in construction");
        assert_eq!(natura("E", Some("Regime forfettario (art. 1)"), false, None), Some("N2.2"));
        assert_eq!(natura("E", None, false, None), Some("N4"));
        assert_eq!(natura("S", None, false, None), None);
    }

    fn sample() -> Fattura {
        let sede = |c: &str| Sede { street: "Via Roma 1".into(), cap: "00100".into(), town: "Roma".into(), province: Some("RM".into()), country: c.into() };
        Fattura {
            transmitter: ("IT".into(), "01234567897".into()),
            progressive: "0000A".into(),
            recipient_code: "ABC1234".into(),
            pec: None,
            seller: Party { name: "Rossi & Figli S.r.l.".into(), vat: Some(("IT".into(), "01234567897".into())), codice_fiscale: None, sede: sede("IT") },
            regime: "RF01".into(),
            rea: Some(("RM".into(), "123456".into(), false)),
            buyer: Party { name: "Bianchi SpA".into(), vat: Some(("IT".into(), "09876543210".into())), codice_fiscale: None, sede: sede("IT") },
            credit_note: false,
            currency: "EUR".into(),
            date: "2026-10-31".into(),
            number: "FT-1".into(),
            stamp_duty: false,
            total: Decimal::new(23200, 2),
            original: None,
            lines: vec![
                Line { description: "Consulenza".into(), quantity: Decimal::ONE, unit: None, total: Decimal::from(100), rate: Decimal::from(22), natura: None },
                Line {
                    description: "Formazione".into(),
                    quantity: Decimal::from(2),
                    unit: Some("ore".into()),
                    total: Decimal::from(110),
                    rate: Decimal::ZERO,
                    natura: Some("N4"),
                },
            ],
            tax: BTreeMap::from([("22.00".to_string(), Decimal::from(22))]),
            due: Some("2026-11-30".into()),
            iban: Some("IT60X0542811101000000123456".into()),
            notes: vec![],
        }
    }

    #[test]
    fn a_fatturapa_as_sdi_takes_it() {
        let f = sample();
        assert!(validate(&f).is_empty(), "{:?}", validate(&f));
        assert_eq!(f.stamped(), Decimal::from(110), "the exempt line bears the stamp duty");
        assert_eq!(f.file_name(), "IT01234567897_0000A.xml");
        let x = to_xml(&f);
        for part in [
            "versione=\"FPR12\"",
            "<CodiceDestinatario>ABC1234</CodiceDestinatario>",
            "<Denominazione>Rossi &amp; Figli S.r.l.</Denominazione></Anagrafica><RegimeFiscale>RF01</RegimeFiscale>",
            "<StatoLiquidazione>LN</StatoLiquidazione>",
            "<TipoDocumento>TD01</TipoDocumento>",
            "<PrezzoUnitario>55.00000000</PrezzoUnitario><PrezzoTotale>110.00</PrezzoTotale><AliquotaIVA>0.00</AliquotaIVA><Natura>N4</Natura>",
            "<DatiRiepilogo><AliquotaIVA>0.00</AliquotaIVA><Natura>N4</Natura><ImponibileImporto>110.00</ImponibileImporto><Imposta>0.00</Imposta><RiferimentoNormativo>",
            "<DatiRiepilogo><AliquotaIVA>22.00</AliquotaIVA><ImponibileImporto>100.00</ImponibileImporto><Imposta>22.00</Imposta><EsigibilitaIVA>I</EsigibilitaIVA></DatiRiepilogo>",
            "<ModalitaPagamento>MP05</ModalitaPagamento><DataScadenzaPagamento>2026-11-30</DataScadenzaPagamento><ImportoPagamento>232.00</ImportoPagamento>",
        ] {
            assert!(x.contains(part), "{part}\nin {x}");
        }
        let mut bad = sample();
        bad.recipient_code = "AB1".into();
        bad.lines[1].natura = None;
        bad.buyer.vat = None;
        assert_eq!(validate(&bad).len(), 3, "{:?}", validate(&bad));
    }
}
