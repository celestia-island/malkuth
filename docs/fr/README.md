# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**Boîte à outils composable de supervision de services pour Rust — JSON-RPC sur des transports enfichables, des workers supervisés, des verrous de coordination et une élection de leader, plus un CLI watchdog.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **Version 0.1.0** — Crate unique, **basé sur tokio**. Le CLI enveloppe
> *n'importe quel* programme (même un qui n'utilise pas la bibliothèque) d'un pool de pods et d'un
> proxy inverse persistant.

Malkuth aide les programmes automatisés et de longue durée à accomplir quatre choses difficiles :

1. **Transport enfichable** — JSON-RPC sur une boucle locale TCP, une
   **WebSocket** distante ou un **IPC** local (sockets Unix / tubes nommés via
   [`interprocess`](https://crates.io/crates/interprocess)). Un seul trait
   `Transport`, distribué selon le schéma d'URL.
2. **Basé sur tokio, léger en frameworks** — le chemin JSON-RPC ne nécessite aucun framework HTTP
   (axum est optionnel, pour les sondes HTTP uniquement).
3. **Fonctionnalités optionnelles et hameçonnables** — la source de sortie, les sondes, les hooks de pulsation et de drainage
   sont des *traits*. Utilisez les valeurs par défaut ou fournissez les vôtres. Un orchestrateur
   `Supervised` « piles incluses » les câble ensemble.
4. **Un CLI watchdog** — `malkuth -- <cmd>` enveloppe un programme avec surveillance de fichiers, un
   pool de pods et un proxy inverse persistant de couche 4.

## Organisation de l'espace de travail
Consultez le [README racine](../../README.md) pour la matrice complète des fonctionnalités et l'utilisation
du CLI, et [Conception](./design/supervision-and-rolling-update.md) pour
l'architecture.
