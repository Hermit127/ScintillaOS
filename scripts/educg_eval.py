#!/usr/bin/env python3
import argparse
import subprocess
import sys
import time
from pathlib import Path


DEFAULT_URL = (
    "https://course.educg.net//pages/contest/contest_submit.jsp?"
    "contestID=Z7zWWwTfti0&my=false&contestCID=0#contestSubAn"
)


def run(cmd, cwd):
    subprocess.run(cmd, cwd=cwd, check=True)


def create_archive(repo, output):
    output.parent.mkdir(parents=True, exist_ok=True)
    run(["git", "archive", "--format=zip", "-o", str(output), "HEAD"], repo)


def body_text(page):
    try:
        return page.locator("body").inner_text(timeout=3000)
    except Exception:
        return ""


def click_submit(page):
    labels = ["提交", "评测", "Submit", "submit"]
    return page.evaluate(
        """labels => {
            const candidates = Array.from(document.querySelectorAll(
                'button,input[type=submit],input[type=button],a'
            ));
            const found = candidates.find(el => {
                const text = `${el.innerText || ''} ${el.value || ''} ${el.textContent || ''}`;
                return labels.some(label => text.includes(label));
            });
            if (!found) return '';
            found.click();
            return found.innerText || found.value || found.textContent || found.tagName;
        }""",
        labels,
    )


def looks_finished(text):
    if "正在评判" in text or "正在评测" in text:
        return False
    done_markers = [
        "评判结束",
        "评测完成",
        "测评完成",
        "运行结束",
        "编译错误",
        "答案错误",
        "Accepted",
        "Wrong Answer",
        "Time Limit",
        "得分",
        "分数",
    ]
    return any(marker in text for marker in done_markers)


def write_snapshot(log_path, text):
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("a", encoding="utf-8") as f:
        f.write("\n\n===== snapshot ")
        f.write(time.strftime("%Y-%m-%d %H:%M:%S"))
        f.write(" =====\n")
        f.write(text)
        if not text.endswith("\n"):
            f.write("\n")


def main():
    parser = argparse.ArgumentParser(
        description="Submit the current HEAD to Educg contest page and poll judge output."
    )
    parser.add_argument("--url", default=DEFAULT_URL)
    parser.add_argument("--repo", default=Path.cwd(), type=Path)
    parser.add_argument("--artifact", type=Path)
    parser.add_argument(
        "--storage-state",
        default=Path.home() / ".educg-playwright-state.json",
        type=Path,
    )
    parser.add_argument("--log", default=Path("tmp/educg-eval.log"), type=Path)
    parser.add_argument("--timeout", default=3600, type=int)
    parser.add_argument("--interval", default=10, type=int)
    parser.add_argument("--headful", action="store_true")
    parser.add_argument(
        "--login",
        action="store_true",
        help="Open a browser, let you log in manually, then save storage state.",
    )
    parser.add_argument(
        "--no-archive",
        action="store_true",
        help="Do not create git archive automatically.",
    )
    args = parser.parse_args()

    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print("Missing dependency: python3 -m pip install playwright && python3 -m playwright install chromium", file=sys.stderr)
        return 2

    repo = args.repo.resolve()
    artifact = args.artifact
    if artifact is None and not args.no_archive:
        artifact = repo / "tmp" / "educg-submit.zip"
        create_archive(repo, artifact)

    with sync_playwright() as p:
        browser = p.chromium.launch(headless=not args.headful)
        context_args = {}
        if args.storage_state.exists():
            context_args["storage_state"] = str(args.storage_state)
        context = browser.new_context(**context_args)
        context.on("dialog", lambda dialog: dialog.accept())
        page = context.new_page()
        page.goto(args.url, wait_until="domcontentloaded")

        if args.login:
            print("Log in in the opened browser, return to the contest submit page, then press Enter here.")
            input()
            args.storage_state.parent.mkdir(parents=True, exist_ok=True)
            context.storage_state(path=str(args.storage_state))
            browser.close()
            return 0

        if artifact is not None:
            file_input = page.locator("input[type=file]")
            if file_input.count() > 0:
                file_input.first.set_input_files(str(artifact.resolve()))

        clicked = click_submit(page)
        if not clicked:
            print("No submit button found. Run with --headful or update selector logic.", file=sys.stderr)
            browser.close()
            return 3

        last_text = ""
        deadline = time.time() + args.timeout
        while time.time() < deadline:
            try:
                page.wait_for_load_state("networkidle", timeout=5000)
            except Exception:
                pass
            text = body_text(page)
            if text and text != last_text:
                print(text.splitlines()[-1] if text.splitlines() else "[page updated]")
                write_snapshot((repo / args.log).resolve(), text)
                last_text = text
            if text and looks_finished(text):
                break
            time.sleep(args.interval)
            try:
                page.reload(wait_until="domcontentloaded")
            except Exception:
                pass

        context.storage_state(path=str(args.storage_state))
        browser.close()
        if time.time() >= deadline:
            print("Timed out while waiting for judge result.", file=sys.stderr)
            return 4
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
