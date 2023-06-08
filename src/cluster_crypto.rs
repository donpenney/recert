use self::{
    cert_key_pair::CertKeyPair,
    crypto_objects::DiscoveredCryptoObect,
    distributed_jwt::DistributedJwt,
    distributed_private_key::DistributedPrivateKey,
    distributed_public_key::DistributedPublicKey,
    keys::{PrivateKey, PublicKey},
    locations::Locations,
};
use crate::{
    cluster_crypto::signee::Signee,
    k8s_etcd::{self, InMemoryK8sEtcd},
    rules::KNOWN_MISSING_PRIVATE_KEY_CERTS,
    rsa_key_pool::{RsaKeyPool, self},
};
use regex::Regex;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use tokio::sync::Mutex;
use x509_certificate::X509CertificateError;

pub(crate) mod cert_key_pair;
pub(crate) mod certificate;
pub(crate) mod crypto_objects;
pub(crate) mod crypto_utils;
pub(crate) mod distributed_cert;
pub(crate) mod distributed_jwt;
pub(crate) mod distributed_private_key;
pub(crate) mod distributed_public_key;
pub(crate) mod jwt;
pub(crate) mod keys;
pub(crate) mod locations;
pub(crate) mod pem_utils;
pub(crate) mod scanning;
pub(crate) mod signee;
pub(crate) mod yaml_crawl;

/// This is the main struct that holds all the crypto objects we've found in the cluster and the
/// locations where we found them, and how they relate to each other.
pub(crate) struct ClusterCryptoObjectsInternal {
    /// At the end of the day we're scanning the entire cluster for private keys, public keys
    /// certificates, and jwts. These four hashmaps is where we store all of them. The reason
    /// they're hashmaps and not vectors is because every one of those objects we encounter might
    /// be found in multiple locations. The value types here (Distributed*) hold a list of
    /// locations where the key/cert was found, and the list of locations for each cert/key grows
    /// as we scan more and more resources. The hashmap keys are of-course hashables so we can
    /// easily check if we already encountered the object before.
    pub(crate) private_keys: HashMap<PrivateKey, Rc<RefCell<DistributedPrivateKey>>>,
    pub(crate) public_keys: HashMap<PublicKey, Rc<RefCell<DistributedPublicKey>>>,
    pub(crate) certs: HashMap<certificate::Certificate, Rc<RefCell<distributed_cert::DistributedCert>>>,
    pub(crate) jwts: HashMap<jwt::Jwt, Rc<RefCell<DistributedJwt>>>,

    /// Every time we encounter a private key, we extract the public key
    /// from it and add to this mapping. This will later allow us to easily
    /// associate certificates with their matching private key (which would
    /// otherwise require brute force search).
    pub(crate) public_to_private: HashMap<PublicKey, PrivateKey>,

    /// After collecting all certs and private keys, we go through the list of certs and try to
    /// find a private key that matches the public key of the cert (with the help of
    /// public_to_private) and populate this list of pairs.
    pub(crate) cert_key_pairs: Vec<Rc<RefCell<CertKeyPair>>>,
}

pub(crate) struct ClusterCryptoObjects {
    internal: Mutex<ClusterCryptoObjectsInternal>,
}

impl ClusterCryptoObjects {
    pub(crate) fn new() -> ClusterCryptoObjects {
        ClusterCryptoObjects {
            internal: Mutex::new(ClusterCryptoObjectsInternal::new()),
        }
    }
    pub(crate) async fn display(&self) {
        self.internal.lock().await.display();
    }
    pub(crate) async fn commit_to_etcd_and_disk(&self, etcd_client: &InMemoryK8sEtcd) {
        self.internal.lock().await.commit_to_etcd_and_disk(etcd_client).await;
    }
    pub(crate) async fn regenerate_crypto(&self, rsa_key_pool: rsa_key_pool::RsaKeyPool) {
        self.internal.lock().await.regenerate_crypto(rsa_key_pool);
    }
    pub(crate) async fn fill_signees(&mut self) {
        self.internal.lock().await.fill_signees();
    }
    pub(crate) async fn pair_certs_and_keys(&mut self) {
        self.internal.lock().await.pair_certs_and_keys();
    }
    pub(crate) async fn associate_public_keys(&mut self) {
        self.internal.lock().await.associate_public_keys();
    }
    pub(crate) async fn fill_cert_key_signers(&mut self) {
        self.internal.lock().await.fill_cert_key_signers();
    }
    pub(crate) async fn fill_jwt_signers(&mut self) {
        self.internal.lock().await.fill_jwt_signers();
    }
    pub(crate) async fn register_discovered_crypto_objects(&mut self, discovered_crypto_objects: Vec<DiscoveredCryptoObect>) {
        self.internal
            .lock()
            .await
            .register_discovered_crypto_objects(discovered_crypto_objects);
    }
}

impl ClusterCryptoObjectsInternal {
    pub(crate) fn new() -> Self {
        ClusterCryptoObjectsInternal {
            private_keys: HashMap::new(),
            public_keys: HashMap::new(),
            certs: HashMap::new(),
            jwts: HashMap::new(),
            public_to_private: HashMap::new(),
            cert_key_pairs: Vec::new(),
        }
    }

    /// Convenience function to display all the crypto objects in the cluster,
    /// their relationships, and their locations.
    pub(crate) fn display(&self) {
        for cert_key_pair in &self.cert_key_pairs {
            if (**cert_key_pair).borrow().signer.as_ref().is_none() {
                println!("{}", (**cert_key_pair).borrow());
            }
        }

        for private_key in self.private_keys.values() {
            println!("{}", (**private_key).borrow());
        }
    }

    /// Commit all the crypto objects to etcd and disk. This is called after all the crypto
    /// objects have been regenerated so that the newly generated objects are persisted in
    /// etcd and on disk.
    async fn commit_to_etcd_and_disk(&mut self, etcd_client: &InMemoryK8sEtcd) {
        for cert_key_pair in &self.cert_key_pairs {
            (**cert_key_pair).borrow().commit_to_etcd_and_disk(etcd_client).await;
        }

        for jwt in self.jwts.values() {
            (**jwt).borrow().commit_to_etcd_and_disk(etcd_client).await;
        }

        for private_key in self.private_keys.values() {
            (**private_key).borrow().commit_to_etcd_and_disk(etcd_client).await;
        }

        for public_key in self.public_keys.values() {
            (**public_key).borrow().commit_to_etcd_and_disk(etcd_client).await;
        }
    }

    /// Recursively regenerate all the crypto objects. This is done by regenerating the top level
    /// cert-key pairs and standalone private keys, which will in turn regenerate all the objects
    /// that depend on them (signees). Requires that first the crypto objects have been paired and
    /// associated through the other methods.
    fn regenerate_crypto(&mut self, mut rsa_key_pool: RsaKeyPool) {
        for cert_key_pair in &self.cert_key_pairs {
            if (**cert_key_pair).borrow().signer.is_some() {
                continue;
            }

            (**cert_key_pair).borrow_mut().regenerate(None, &mut rsa_key_pool)
        }

        for private_key in self.private_keys.values() {
            (**private_key).borrow_mut().regenerate(&mut rsa_key_pool)
        }

        println!("- Regeneration complete, verifying...");
        self.assert_regeneration();
    }

    fn assert_regeneration(&mut self) {
        // Assert all known objects have been regenerated.
        for cert_key_pair in &self.cert_key_pairs {
            let signer = &(*(**cert_key_pair).borrow()).signer;
            if let Some(signer) = signer {
                assert!(
                    (**signer).borrow().regenerated,
                    "Didn't seem to regenerate signer with cert at {} and keys at {} while I'm at {} with keys at {}",
                    (*(**signer).borrow().distributed_cert).borrow().locations,
                    if let Some(key) = &(**signer).borrow().distributed_private_key {
                        format!("{}", (*key).borrow().locations)
                    } else {
                        "None".to_string()
                    },
                    (*(**cert_key_pair).borrow().distributed_cert).borrow().locations,
                    if let Some(key) = &(**cert_key_pair).borrow().distributed_private_key {
                        format!("{}", (*key).borrow().locations)
                    } else {
                        "None".to_string()
                    },
                );

                assert!(
                    (**signer).borrow().signees.len() > 0,
                    "Zero signees signer with cert at {} and keys at {}",
                    (*(**signer).borrow().distributed_cert).borrow().locations,
                    if let Some(key) = &(**signer).borrow().distributed_private_key {
                        format!("{}", (*key).borrow().locations)
                    } else {
                        "None".to_string()
                    },
                );

                for signee in &(**signer).borrow().signees {
                    match signee {
                        signee::Signee::CertKeyPair(pair) => {
                            assert!(
                                (*pair).borrow().regenerated,
                                "Didn't seem to regenerate cert-key pair {} signee of {}",
                                (*pair).borrow(),
                                (**signer).borrow(),
                            );
                        }
                        signee::Signee::Jwt(jwt) => {
                            assert!(
                                (*jwt).borrow().regenerated,
                                "Didn't seem to regenerate jwt {:#?} signee of {}",
                                (*jwt).borrow(),
                                (**signer).borrow(),
                            );
                        }
                    }
                }

                // Assert our cert-key pair is in the signees of the signer.
                assert!(
                    (**signer).borrow().signees.contains(&Signee::CertKeyPair(cert_key_pair.clone())),
                    "Signer {} doesn't have cert-key pair {} as a signee",
                    (**signer).borrow(),
                    (**cert_key_pair).borrow(),
                );
            }

            assert!(
                (**cert_key_pair).borrow().regenerated,
                "Didn't seem to regenerate cert at {}",
                (*(**cert_key_pair).borrow().distributed_cert).borrow().locations,
            );
        }
        for distributed_public_key in self.public_keys.values() {
            assert!(
                (*distributed_public_key).borrow().regenerated,
                "Didn't seem to regenerate public key {}",
                (**distributed_public_key).borrow(),
            );
        }
        for distributed_jwt in self.jwts.values() {
            assert!(
                (*distributed_jwt).borrow().regenerated,
                "Didn't seem to regenerate jwt {:#?}",
                (*distributed_jwt).borrow(),
            );
        }
        for distributed_private_key in self.private_keys.values() {
            assert!(
                (*distributed_private_key).borrow().regenerated,
                "Didn't seem to regenerate private key {}",
                (*distributed_private_key).borrow(),
            );
        }
        assert_eq!(self.certs.len(), 0);
    }

    fn fill_cert_key_signers(&mut self) {
        for cert_key_pair in &self.cert_key_pairs {
            let mut true_signing_cert: Option<Rc<RefCell<CertKeyPair>>> = None;
            if !(*(**cert_key_pair).borrow().distributed_cert)
                .borrow()
                .certificate
                .original
                .subject_is_issuer()
            {
                for potential_signing_cert_key_pair in &self.cert_key_pairs {
                    match (*(**cert_key_pair).borrow().distributed_cert)
                        .borrow()
                        .certificate
                        .original
                        .verify_signed_by_certificate(
                            &(*(*potential_signing_cert_key_pair).borrow().distributed_cert)
                                .borrow()
                                .certificate
                                .original,
                        ) {
                        Ok(_) => true_signing_cert = Some(Rc::clone(&potential_signing_cert_key_pair)),
                        Err(err) => match err {
                            X509CertificateError::CertificateSignatureVerificationFailed => {}
                            X509CertificateError::UnsupportedSignatureVerification(..) => {
                                // This is a hack to get around the fact this lib doesn't support
                                // all signature algorithms yet.
                                if crypto_utils::openssl_is_signed(&potential_signing_cert_key_pair, &cert_key_pair) {
                                    true_signing_cert = Some(Rc::clone(&potential_signing_cert_key_pair));
                                }
                            }
                            _ => panic!("Error verifying signed by certificate: {:?}", err),
                        },
                    }
                }

                if true_signing_cert.is_none() {
                    panic!(
                        "No signing cert found for {}",
                        (*(**cert_key_pair).borrow().distributed_cert).borrow().locations
                    );
                }
            }

            (**cert_key_pair).borrow_mut().signer = true_signing_cert;
        }
    }

    /// For every jwt, find the private key that signed it (or certificate key pair that signed it,
    /// although rare in OCP) and record it. This will later be used to know how to regenerate the
    /// jwt.
    fn fill_jwt_signers(&mut self) {
        // Usually it's just one private key signing all the jwts, so to speed things up, we record
        // the last signer and use that as the first guess for the next jwt. This dramatically
        // speeds up the process of finding the signer for each jwt, as trying all private keys is
        // very slow, especially in debug mode without optimizations.
        let mut last_signer: Option<Rc<RefCell<DistributedPrivateKey>>> = None;

        for distributed_jwt in self.jwts.values() {
            let mut maybe_signer = jwt::JwtSigner::Unknown;

            if let Some(last_signer) = &last_signer {
                match crypto_utils::verify_jwt(&PublicKey::from(&(*last_signer).borrow().key), &(**distributed_jwt).borrow()) {
                    Ok(_claims /* We don't care about the claims, only that the signature is correct */) => {
                        maybe_signer = jwt::JwtSigner::PrivateKey(Rc::clone(&last_signer));
                    }
                    Err(_error) => {}
                }
            } else {
                for distributed_private_key in self.private_keys.values() {
                    match crypto_utils::verify_jwt(
                        &PublicKey::from(&(**distributed_private_key).borrow().key),
                        &(**distributed_jwt).borrow(),
                    ) {
                        Ok(_claims /* We don't care about the claims, only that the signature is correct */) => {
                            maybe_signer = jwt::JwtSigner::PrivateKey(Rc::clone(distributed_private_key));
                            last_signer = Some(Rc::clone(&distributed_private_key));
                            break;
                        }
                        Err(_error) => {}
                    }
                }
            }

            match &maybe_signer {
                jwt::JwtSigner::Unknown => {
                    for cert_key_pair in &self.cert_key_pairs {
                        if let Some(distributed_private_key) = &(**cert_key_pair).borrow().distributed_private_key {
                            match crypto_utils::verify_jwt(
                                &PublicKey::from(&(**distributed_private_key).borrow().key),
                                &(**distributed_jwt).borrow(),
                            ) {
                                Ok(_claims /* We don't care about the claims, only that the signature is correct */) => {
                                    maybe_signer = jwt::JwtSigner::CertKeyPair(Rc::clone(cert_key_pair));
                                    break;
                                }
                                Err(_error) => {}
                            }
                        }
                    }
                }
                _ => {}
            }

            if maybe_signer == jwt::JwtSigner::Unknown {
                panic!("JWT has unknown signer");
            }

            (**distributed_jwt).borrow_mut().signer = maybe_signer;
        }
    }

    /// For every cert-key pair or private key, find all the crypto objects that depend on it and
    /// record them. This will later be used to know how to regenerate the crypto objects.
    fn fill_signees(&mut self) {
        for cert_key_pair in &self.cert_key_pairs {
            let mut signees = Vec::new();
            for potential_signee in &self.cert_key_pairs {
                if let Some(potential_signee_signer) = &(**potential_signee).borrow().signer {
                    if (*(**potential_signee_signer).borrow().distributed_cert)
                        .borrow()
                        .certificate
                        .original
                        == (*(**cert_key_pair).borrow().distributed_cert).borrow().certificate.original
                    {
                        signees.push(signee::Signee::CertKeyPair(Rc::clone(&potential_signee)));
                    }
                }
            }
            for potential_jwt_signee in self.jwts.values() {
                match &(*potential_jwt_signee).borrow_mut().signer {
                    jwt::JwtSigner::Unknown => panic!("JWT has unknown signer"),
                    jwt::JwtSigner::CertKeyPair(jwt_signer_cert_key_pair) => {
                        if jwt_signer_cert_key_pair == cert_key_pair {
                            signees.push(signee::Signee::Jwt(Rc::clone(potential_jwt_signee)));
                        }
                    }
                    jwt::JwtSigner::PrivateKey(_) => {}
                }
            }

            (**cert_key_pair).borrow_mut().signees = signees;
        }

        for distributed_private_key in self.private_keys.values() {
            for potential_jwt_signee in self.jwts.values() {
                match &(**potential_jwt_signee).borrow_mut().signer {
                    jwt::JwtSigner::Unknown => panic!("JWT has unknown signer"),
                    jwt::JwtSigner::CertKeyPair(_cert_key_pair) => {}
                    jwt::JwtSigner::PrivateKey(jwt_signer_distributed_private_key) => {
                        if jwt_signer_distributed_private_key == distributed_private_key {
                            (**distributed_private_key)
                                .borrow_mut()
                                .signees
                                .push(signee::Signee::Jwt(Rc::clone(potential_jwt_signee)));
                        }
                    }
                }
            }
        }
    }

    /// Find the private key associated with the subject of each certificate and combine them into
    /// a cert-key pair. Also remove the private key from the list of private keys as it is now
    /// part of a cert-key pair, the remaining private keys are considered standalone.
    fn pair_certs_and_keys(&mut self) {
        let mut paired_cers_to_remove = vec![];
        for (hashable_cert, distributed_cert) in &self.certs {
            let pair = Rc::new(RefCell::new(cert_key_pair::CertKeyPair {
                distributed_private_key: None,
                distributed_cert: Rc::clone(distributed_cert),
                signer: None,
                signees: Vec::new(),
                associated_public_key: None,
                regenerated: false,
            }));

            let subject_public_key = (**distributed_cert).borrow().certificate.public_key.clone();
            if let Occupied(private_key) = self.public_to_private.entry(subject_public_key.clone()) {
                if let Occupied(distributed_private_key) = self.private_keys.entry(private_key.get().clone()) {
                    (*pair).borrow_mut().distributed_private_key = Some(Rc::clone(distributed_private_key.get()));

                    // Remove the private key from the pool of private keys as it's now paired with a cert
                    self.private_keys.remove(&private_key.get());
                } else {
                    panic!("Private key not found");
                }
            } else if KNOWN_MISSING_PRIVATE_KEY_CERTS.contains(&(**distributed_cert).borrow().certificate.subject)
                || KNOWN_MISSING_PRIVATE_KEY_CERTS.iter().any(|known_missing_private_key_cert| {
                    let re = Regex::new(known_missing_private_key_cert).unwrap();
                    re.is_match(&(**distributed_cert).borrow().certificate.subject)
                })
            {
                // This is a known missing private key cert, so we don't need to panic about it not
                // having a private key.
            } else {
                panic!(
                    "Private key not found for cert not in KNOWN_MISSING_PRIVATE_KEY_CERTS, cannot continue, {}. The cert was found in {}",
                    (**distributed_cert).borrow().certificate.subject,
                    (**distributed_cert).borrow().locations,
                );
            }

            paired_cers_to_remove.push(hashable_cert.clone());
            self.cert_key_pairs.push(pair);
        }

        for paired_cer_to_remove in paired_cers_to_remove {
            self.certs.remove(&paired_cer_to_remove);
        }
    }

    /// Associate public keys with their cert-key pairs or standalone private keys.
    fn associate_public_keys(&mut self) {
        for cert_key_pair in &self.cert_key_pairs {
            if let Occupied(public_key_entry) = self.public_keys.entry(
                (*(**cert_key_pair).borrow().distributed_cert)
                    .borrow()
                    .certificate
                    .public_key
                    .clone(),
            ) {
                (*cert_key_pair).borrow_mut().associated_public_key = Some(Rc::clone(public_key_entry.get()));
            }
        }

        for distributed_private_key in self.private_keys.values() {
            let public_part = PublicKey::from(&(*distributed_private_key).borrow().key);

            if let Occupied(public_key_entry) = self.public_keys.entry(public_part) {
                (*distributed_private_key).borrow_mut().associated_distributed_public_key = Some(Rc::clone(public_key_entry.get()));
            }
        }
    }

    pub(crate) fn register_discovered_crypto_objects(&mut self, discovered_crypto_objects: Vec<DiscoveredCryptoObect>) {
        for discovered_crypto_object in discovered_crypto_objects {
            let location = discovered_crypto_object.location.clone();
            match discovered_crypto_object.crypto_object {
                crypto_objects::CryptoObject::PrivateKey(private_part, public_part) => {
                    self.public_to_private.insert(public_part, private_part.clone());

                    match self.private_keys.entry(private_part.clone()) {
                        Vacant(distributed_private_key_entry) => {
                            distributed_private_key_entry.insert(Rc::new(RefCell::new(distributed_private_key::DistributedPrivateKey {
                                locations: Locations(vec![location.clone()].into_iter().collect()),
                                key: private_part,
                                signees: vec![],
                                // We don't set the public key here even though we just generated it because
                                // this field is for actual public keys that we find in the wild, not ones we
                                // generate ourselves.
                                associated_distributed_public_key: None,
                                regenerated: false,
                            })));
                        }

                        Occupied(distributed_private_key_entry) => {
                            (**distributed_private_key_entry.into_mut())
                                .borrow_mut()
                                .locations
                                .0
                                .insert(location.clone());
                        }
                    }
                }
                crypto_objects::CryptoObject::PublicKey(public_key) => match self.public_keys.entry(public_key.clone()) {
                    Vacant(distributed_public_key_entry) => {
                        distributed_public_key_entry.insert(Rc::new(RefCell::new(distributed_public_key::DistributedPublicKey {
                            locations: Locations(vec![location.clone()].into_iter().collect()),
                            key: public_key,
                            regenerated: false,
                        })));
                    }

                    Occupied(distributed_public_key_entry) => {
                        (**distributed_public_key_entry.into_mut())
                            .borrow_mut()
                            .locations
                            .0
                            .insert(location.clone());
                    }
                },
                crypto_objects::CryptoObject::Certificate(hashable_cert) => match self.certs.entry(hashable_cert.clone()) {
                    Vacant(distributed_cert) => {
                        distributed_cert.insert(Rc::new(RefCell::new(distributed_cert::DistributedCert {
                            certificate: hashable_cert,
                            locations: Locations(vec![location.clone()].into_iter().collect()),
                        })));
                    }
                    Occupied(distributed_cert) => {
                        (**distributed_cert.get()).borrow_mut().locations.0.insert(location.clone());
                    }
                },
                crypto_objects::CryptoObject::Jwt(jwt) => match self.jwts.entry(jwt.clone()) {
                    Vacant(distributed_jwt) => {
                        distributed_jwt.insert(Rc::new(RefCell::new(distributed_jwt::DistributedJwt {
                            jwt,
                            locations: Locations(vec![location].into_iter().collect()),
                            signer: jwt::JwtSigner::Unknown,
                            regenerated: false,
                        })));
                    }
                    Occupied(distributed_jwt) => {
                        (**distributed_jwt.get()).borrow_mut().locations.0.insert(location);
                    }
                },
            }
        }
    }
}
