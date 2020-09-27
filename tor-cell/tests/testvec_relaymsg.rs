/// Example relay messages to encode and decode.
///
/// Except where noted, these were taken by instrumenting Tor
/// 0.4.5.0-alpha-dev to dump all of its cells to the logs, and
/// running in a chutney network with "test-network-all".
use tor_cell::relaycell::{msg, RelayCmd};
use tor_llcrypto::pk::rsa::RSAIdentity;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use hex_literal::hex;

fn msg(cmd: RelayCmd, s: &str, msg: &msg::RelayMsg) {
    assert_eq!(msg.get_cmd(), cmd);
    let body = {
        let mut s = s.to_string();
        s.retain(|c| !c.is_whitespace());
        hex::decode(s).unwrap()
    };
    let mut r = tor_bytes::Reader::from_slice(&body[..]);

    let decoded = msg::RelayMsg::decode_from_reader(cmd, &mut r).unwrap();

    // This is a bit kludgey: we don't implement PartialEq for
    // messages, but we do implement Debug.  That actually seems a
    // saner arrangement to me as of this writing.
    assert_eq!(format!("{:?}", decoded), format!("{:?}", msg));

    let mut encoded1 = Vec::new();
    let mut encoded2 = Vec::new();
    decoded.encode_onto(&mut encoded1);
    msg.clone().encode_onto(&mut encoded2);
    assert_eq!(encoded1, encoded2);
    assert_eq!(body, encoded2);
}

#[test]
fn test_begin() {
    let cmd = RelayCmd::BEGIN;
    assert_eq!(Into::<u8>::into(cmd), 1_u8);

    msg(
        cmd,
        "3132372E302E302E313A3730303300",
        &msg::Begin::new("127.0.0.1", 7003, 0).unwrap().into(),
    );

    // hand-generated test, with flags set.
    msg(
        cmd,
        "7777772e786b63642e636f6d3a34343300 00000003",
        &msg::Begin::new("www.xkcd.com", 443, 3).unwrap().into(),
    );

    // hand-generated test, with IPv6 set.
    msg(
        cmd,
        "5b323030313a6462383a3a315d3a323200",
        &msg::Begin::new("2001:db8::1", 22, 0).unwrap().into(),
    );
}

#[test]
fn test_begindir() {
    let cmd = RelayCmd::BEGIN_DIR;
    assert_eq!(Into::<u8>::into(cmd), 13_u8);

    msg(cmd, "", &msg::RelayMsg::BeginDir);
}

#[test]
fn test_connected() {
    let cmd = RelayCmd::CONNECTED;
    assert_eq!(Into::<u8>::into(cmd), 4_u8);

    msg(cmd, "", &msg::Connected::new_empty().into());
    let localhost = "127.0.0.1".parse::<IpAddr>().unwrap();
    msg(
        cmd,
        "7F000001 00000E10",
        &msg::Connected::new_with_addr(localhost, 0xe10).into(),
    );
}

#[test]
fn test_drop() {
    let cmd = RelayCmd::DROP;
    assert_eq!(Into::<u8>::into(cmd), 10_u8);

    msg(cmd, "", &msg::RelayMsg::Drop);
}

#[test]
fn test_end() {
    let cmd = RelayCmd::END;
    assert_eq!(Into::<u8>::into(cmd), 3_u8);

    msg(cmd, "01", &msg::End::new_misc().into());
    msg(cmd, "06", &msg::End::new_with_reason(6).into());

    // hand-generated, for exit policy rejections
    let localhost = "127.0.0.7".parse::<IpAddr>().unwrap();
    msg(
        cmd,
        "04 7f000007 00000100",
        &msg::End::new_exitpolicy(localhost, 256).into(),
    );
    let addr = "2001:db8::f00b".parse::<IpAddr>().unwrap();
    msg(
        cmd,
        "04 20010db8 00000000 00000000 0000f00b 00000200",
        &msg::End::new_exitpolicy(addr, 512).into(),
    );
}

#[test]
fn test_extend2() {
    let cmd = RelayCmd::EXTEND2;
    assert_eq!(Into::<u8>::into(cmd), 14_u8);

    // TODO: test one with more link specifiers.

    let body =
        "02
         00 06 7F0000011388
         02 14 03479E93EBF3FF2C58C1C9DBF2DE9DE9C2801B3E
         0002 0054
         03479E93EBF3FF2C58C1C9DBF2DE9DE9C2801B3E36A3E0BDB1F1A10579820243DD2E228B902940EB49EF842646E3926768C28410074BEE50A535116D657C6BFB7E5114119DC69F05DF907EF62002EA782B0A357F";

    let handshake = hex::decode("03479E93EBF3FF2C58C1C9DBF2DE9DE9C2801B3E36A3E0BDB1F1A10579820243DD2E228B902940EB49EF842646E3926768C28410074BEE50A535116D657C6BFB7E5114119DC69F05DF907EF62002EA782B0A357F").unwrap();
    let rsa =
        RSAIdentity::from_bytes(&hex::decode("03479E93EBF3FF2C58C1C9DBF2DE9DE9C2801B3E").unwrap())
            .unwrap();
    let addr = "127.0.0.1:5000".parse::<SocketAddr>().unwrap();

    let mut ls = Vec::new();
    ls.push(addr.into());
    ls.push(rsa.into());
    msg(cmd, body, &msg::Extend2::new(ls, 2, handshake).into());
}

#[test]
fn test_extend() {
    let cmd = RelayCmd::EXTEND;
    assert_eq!(Into::<u8>::into(cmd), 6_u8);

    let body = "7F000001 138C
                71510FC729E1DBE35586F0031D69A38FC684B26D657821EE640C299BA9F8FD38D3A3376F2DD3A79A0B73836AB4B42E5FB3BEE1383F3184A852B292626DCC64AF672A8FAEFC263C38370768EF9EA6C244BA079142D3E23835F6914DE0C7F468316C4265E109F5312987275D61E1DC831A3323195DDE70841CEE2DC30F6DCDBDABA40A75FDFB714431FC5EB8F84D4150EE2C2478A79018F18D7F30F6BB677516CF03390F5180B371DEAEBB89175798864D2130B13ED1D20B254F07
                CF555174CBE8AD62A7E764A8F3D85D40C5145ABB";
    let addr = "127.0.0.1".parse::<Ipv4Addr>().unwrap();
    let handshake = hex::decode("71510FC729E1DBE35586F0031D69A38FC684B26D657821EE640C299BA9F8FD38D3A3376F2DD3A79A0B73836AB4B42E5FB3BEE1383F3184A852B292626DCC64AF672A8FAEFC263C38370768EF9EA6C244BA079142D3E23835F6914DE0C7F468316C4265E109F5312987275D61E1DC831A3323195DDE70841CEE2DC30F6DCDBDABA40A75FDFB714431FC5EB8F84D4150EE2C2478A79018F18D7F30F6BB677516CF03390F5180B371DEAEBB89175798864D2130B13ED1D20B254F07").unwrap();
    dbg!(handshake.len());
    let rsa =
        RSAIdentity::from_bytes(&hex::decode("CF555174CBE8AD62A7E764A8F3D85D40C5145ABB").unwrap())
            .unwrap();

    msg(
        cmd,
        body,
        &msg::Extend::new(addr, 5004, handshake, rsa).into(),
    );
}

#[test]
fn test_extended2() {
    let cmd = RelayCmd::EXTENDED2;
    assert_eq!(Into::<u8>::into(cmd), 15_u8);

    let body = "0040 0026619058EB2661834D54C2624828728F28915587CDAD1AD3373B85F33A480EEA9D3B2CEF8D39C1DBD2FA519E75296B96960690C79A28D6A0D9454F8E9634BD";
    let handshake = hex::decode("0026619058EB2661834D54C2624828728F28915587CDAD1AD3373B85F33A480EEA9D3B2CEF8D39C1DBD2FA519E75296B96960690C79A28D6A0D9454F8E9634BD").unwrap();

    msg(cmd, body, &msg::Extended2::new(handshake).into());
}

#[test]
fn test_extended() {
    let cmd = RelayCmd::EXTENDED;
    assert_eq!(Into::<u8>::into(cmd), 7_u8);

    let body = "2B079274DEB8B0A03F7BCCA65813FF557ECB6362C44BE4AC0374E5255540D2712ADBE0E858FD433DD2EB473D85D3C69A457DDE9B7F28E95833EDA57416B9409B68271FFF420F57C53EC1B823491C543C69D06A56A20AB95DD595EE2B16F1AAB24E6314E36D80DF76A67970263AC4902DE692A6AF2FE0B16DF6A9E9124675FAB94A4CF7D65D0F3EBA05682F9DC76A2C47DD3566B3";

    let handshake = hex::decode(body).unwrap();

    msg(cmd, body, &msg::Extended::new(handshake).into());
}

#[test]
fn test_truncated() {
    let cmd = RelayCmd::TRUNCATED;
    assert_eq!(Into::<u8>::into(cmd), 9_u8);

    msg(cmd, "08", &msg::Truncated::new(8).into());
}

/* For circuit padding:

fn test_padding_negotiate() {
    let cmd = RelayCmd::PADDING_NEGOTIATE;
    assert_eq!(Into::<u8>::into(cmd), 41_u8);

    msg(cmd, "0002000000000001", ... );
}

PADDING_NEGOTIATED, 42, "0001010000000001"
PADDING_NEGOTIATED, 42, "0002010000000001"
PADDING_NEGOTIATED, 42, "0002010100000001"
*/

/*  For onion services only:

ESTABLISH_INTRO, 32, "008C30818902818100D2419B56BFB89D35EE9EB6FD328EDE897C29DA6DF68E589812D2EEC030C55A56FB010E06097A0A93EEDD8DE351A32DAAF5C7B232DC22E549EF25E8CF5E338C1C12C7828624E61B2700E931B7D532951E8907A477720B087840B7AD9D487D9F1AFBEAEAD2A7C3D9D1EB0E579FFEB9AC2BAA181FE76397D299C469B46969906BD9020301000169529D1D09554CA1C45083A7DDC96BAE22146FC09BD3B9266D17CDEE66EB2D0B7ABBC828ED300BEC8851A2178AFE0FC671D7CC7A7C0A36BE854BBD6AAD7AF4C44F32B804788B5EDA2C0AB041E61AC6C901DCB212356E8D2A00463D6A5B17C1A2DAA409A8E926FAF6592A8C7CF2B45FD8C4A218595016BF52098878FD6B1EDB11D91D32D1B62DED57AB67AB69886E1374B56CFB9E"

    ESTABLISH_INTRO, 32, "0200200450EFF847E40C180888F5EB9179F9B59F043385834B4C373C328E12FDFF46B400546514E3BA58E95409828A235B6390B3729B4CB8E8607024081C860E1A0D40DC0040AF031A801FE9822853D4674C5061B0352F7E2487415E25E25554C0DBF88146BCA9EBD2BD62338ADF3CC217658110EF38DB505C77B2FB38A1C0AF3C22C948F604"

ESTABLISH_RENDEZVOUS, 33, "4AA2BAF815CEBB4B922B5BF6F545AEE0DDBE1254"

INTRODUCE1, 34, "000000000000000000000000000000000000000002002011AAA1BA28353424061A63326C3BFF4FE4EC7EC24BB5EB740E9D167DEF103206001E99C1B9B0C24E6C63E9A182C33A8969DF94D08FD9FE98D82E21B262AE6CBC4E58971D6A38426C5D52519AB03377274AF74D2332C8FB599BFCBC5F2B02ECDAC4025EE24B63A824101E29EB9917B1C3A72E16FA2336DC81D0D483D31B9181F7CF628272FA7988301ADCE3B400880C25633718503B10C9F8909DFA50AEEDB90D7FD9D0090DBFAC4E61D527F97FD8C0A8BAB3DABF9A5E3C7FCD5FF843905F2C401743F5019B04A2F1427D3B2343A388846B7EACDD9FD46A24776D0DDDE3696A90EE36DC4732A95BFAFF871AEDE3BB5DD86047905B716148A27FF3C8A5B0F5282DD5430DA7E2A421D5274281EAC6C0EAE94EB17996EE81953FB700DD6D1DC4B7"

INTRODUCE1, 34, "3AA272C26715FC3EEFE93C1CEE9ED90ACF702221338BE4032FF7CC8D7B8CF0EFB08FF51C3BCE0A289047ADF9BE51129E5FA81D40307B69577DCC3A899827D4287B00BDD661CA90BE2045D0A86FFEBF4EB9D3135B971B40D587D82F04E00EB3ED0CE3BBD70B2FF32D3BF08AD1A56EFC7C16461D7487506BC58319BFEFE936AAB18B0ACD308EF7F830370CC52A6115819EFE2BED3FC1234EDDF21321AE9D495AD7923BB0B6F275F19B636010D4E46468C366E20C5581730896BB6E2E684C75412E25A27924ED5F682122C0A7F26099E97531212558CD5E289C9CB72D1E09304037EC6A856B50B65642C20E307C4F201392C70764F3DB8AB4F6BC02C546D5FBAD4C3A4426347EECA864A990201E4AA8538104A9ABCA33716E900A7FE6A480FD40735E36C473B943E1BAA7A201397608E0C2C6B96D453B22B1C2A337463FD6BA89E9636FF6AFF0DD19FD121AB4F6605D8102E814FF1C7A31F3A07B1C49FEEC3A89299E60138349C3555D61884F35F3947A49D0E8359D72F7B6D0FCD1D8A489E668AE8A6DB3461A24109323E9BC19FF72413E"

INTRODUCE_ACK, 40, ""
INTRODUCE_ACK, 40, "000000"

INTRO_ESTABLISHED, 38, "00"

RENDEZVOUS1, 36, "1757F14E8378746756B5F91D7898084F646DCF5E537657DE71774B44A3A9840BB3D881F24792B74758D67B7B95468537CF9C706992F76AA87E0AC278C3B75B4D747251F7C92CC2FB900249A316771D82B95F669A85C38AC0AD511D00661DA38B060BD297FCC6719EAD00C0301619E20B48B1F150FA42311511E5E683C34C8CD81273683190B018239554956BDB1B8AAE0951D2B5C32CF0EB2B4CC498FABE347DF1B6F273D0E976FE"


RENDEZVOUS2, 37, "2CBF819E67317EE501880FBE18515C440FB2F6AEA5D7B4349EFC478A714C237309C0FB63B35DC820513A0DEBED469B3607C06A2B7875B6394019C1081954AA1F77C141C7F4B9772D4026D3F2567CE4BAAC589E4DEACE285A33F12BEFF16FEF6120DBB2E0B1BCF78B2E765DB23464EABA3FC6C5126D551BAA32F7A179AB1BD888E0CB8F1E9CA0D8CE6583C144A551D564652829BA"

RENDEZVOUS_ESTABLISHED, 39, ""

 */

#[test]
fn test_resolve() {
    // these values are hand-generated.

    let cmd = RelayCmd::RESOLVE;
    assert_eq!(Into::<u8>::into(cmd), 11_u8);

    let body = hex::encode(b"www.torproject.org\0");
    msg(cmd, &body, &msg::Resolve::new("www.torproject.org").into());

    let body = hex::encode(b"1.0.0.127.in-addr.arpa\0");
    let addr = "127.0.0.1".parse::<IpAddr>().unwrap();
    msg(cmd, &body, &msg::Resolve::new_reverse(&addr).into());

    let body = hex::encode(
        &b"0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.1.0.0.2.ip6.arpa\0"[..],
    );
    let addr = "2001::".parse::<IpAddr>().unwrap();
    msg(cmd, &body, &msg::Resolve::new_reverse(&addr).into());
}

#[test]
fn test_resolved() {
    // these values are hand-generated.
    let cmd = RelayCmd::RESOLVED;
    assert_eq!(Into::<u8>::into(cmd), 12_u8);

    msg(cmd, "", &msg::Resolved::new_empty().into());
    msg(
        cmd,
        "F0 00 00000E10",
        &msg::Resolved::new_err(true, 3600).into(),
    );
    msg(
        cmd,
        "F1 00 00000E10",
        &msg::Resolved::new_err(false, 3600).into(),
    );

    let mut r = msg::Resolved::new_empty();
    r.add_answer(msg::ResolvedVal::Ip("127.0.0.1".parse().unwrap()), 3600);
    r.add_answer(msg::ResolvedVal::Ip("127.0.0.2".parse().unwrap()), 7200);
    msg(
        cmd,
        "04 04 7f000001 00000E10 04 04 7f000002 00001c20",
        &r.into(),
    );

    let mut r = msg::Resolved::new_empty();
    r.add_answer(msg::ResolvedVal::Ip("1234::5678".parse().unwrap()), 128);
    msg(
        cmd,
        "06 10 12340000000000000000000000005678 00000080",
        &r.into(),
    );

    let mut r = msg::Resolved::new_empty();
    r.add_answer(msg::ResolvedVal::Hostname("www.torproject.org".into()), 600);
    msg(
        cmd,
        "00 12 7777772e746f7270726f6a6563742e6f7267 00000258",
        &r.into(),
    );
}

#[test]
fn test_sendme() {
    // these values are hand-generated.
    let cmd = RelayCmd::SENDME;
    assert_eq!(Into::<u8>::into(cmd), 5_u8);

    msg(cmd, "", &msg::Sendme::new_empty().into());

    let tag = hex!("F01234989823478bcdefabcdef01234567890123");
    msg(
        cmd,
        "01 0014 F01234989823478bcdefabcdef01234567890123",
        &msg::Sendme::new_tag(tag).into(),
    )
}

#[test]
fn test_truncate() {
    let cmd = RelayCmd::TRUNCATE;
    assert_eq!(Into::<u8>::into(cmd), 8_u8);

    msg(cmd, "", &msg::RelayMsg::Truncate);
}

// TODO: need to add tests for:
//    - unrecognized
//    - data
