# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**Rust용 컴포저블 서비스 감독 툴킷 —— 플러그 가능한 트랜스포트 위의 JSON-RPC, 감독되는 워커, 조정 잠금과 리더 선출, 그리고 watchdog CLI.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **버전 0.1.0** —— 단일 크레이트, **tokio 기반**. CLI는 *어떤* 프로그램이든
> (라이브러리를 사용하지 않는 프로그램도) pod 풀과
> 스티키 리버스 프록시로 감쌉니다.

Malkuth는 자동화되어 장시간 실행되는 프로그램이 네 가지 어려운 일을 해결하도록 돕습니다.

1. **플러그 가능한 트랜스포트** —— 로컬 TCP 루프백, 원격
   **WebSocket** 또는 로컬 **IPC**([`interprocess`](https://crates.io/crates/interprocess) 기반
   유닉스 소켓 / 명명된 파이프) 위의 JSON-RPC. 단일 `Transport`
   트레이트를 URL 스킴으로 디스패치합니다.
2. **tokio 기반, 프레임워크 경량** —— JSON-RPC 경로는 HTTP 프레임워크가 필요 없습니다
   (axum은 선택 사항이며 HTTP 프로브 전용입니다).
3. **선택적이고 훅 가능한 기능** —— 종료 소스, 프로브, 하트비트와 드레인
   훅은 *트레이트*입니다. 기본값을 사용하거나 직접 제공하세요. 배터리 포함
   `Supervised` 오케스트레이터가 이들을 연결해 줍니다.
4. **watchdog CLI** —— `malkuth -- <cmd>`는 프로그램을 파일 감시,
   pod 풀, L4 스티키 리버스 프록시로 감쌉니다.

## 워크스페이스 구성
전체 기능 매트릭스와 CLI 사용법은 [루트 README](../../README.md)를,
아키텍처는 [설계](./design/supervision-and-rolling-update.md)를 참조하세요.
