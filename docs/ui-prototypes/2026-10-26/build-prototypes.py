from pathlib import Path
import re

root = Path(__file__).resolve().parent
source = (root / "index.html").read_text(encoding="utf-8")
css = (root / "termius-style.css").read_text(encoding="utf-8")
study = (root / "termius-study.js").read_text(encoding="utf-8")

source = source.replace(
    "<title>Adit · 工作台原型对比</title>",
    "<title>Adit · Termius 风格原型 D / E / F</title>",
)
source = source.replace("ADIT / INTERFACE STUDIES", "ADIT / DARK WORKSPACE STUDIES")
source = source.replace("让连接有秩序，让工作区安静", "Adit · Termius 风格探索")
source = source.replace("<b>A</b>资源与工作分离", "<b>D</b>资源卡片库")
source = source.replace("<b>B</b>连接树与有限标签", "<b>E</b>专注终端")
source = source.replace("<b>C</b>任务工作区与分屏", "<b>F</b>任务分屏")
source = source.replace('href="references.html"', 'href="termius-references.html"')
source = source.replace("</style>", css + "\n</style>", 1)

for name in (
    "titleBar", "activity", "renderSidebar", "tabStrip", "catalogStage",
    "contentHead", "tools", "renderModal", "footer", "render",
):
    declaration = "function " + name + "("
    assert source.count(declaration) == 1, name
    source = source.replace(
        declaration, "function legacy" + name[0].upper() + name[1:] + "(", 1
    )

source = source.replace("${state.mode==='b'?'classic':''}", "")
source, count = re.subn(
    r"""\+\(state\.mode==='b'\?'<div class="classic-menu">.*?</div>':''\)\+""",
    "+", source,
)
assert count == 1
source = source.replace("state.theme=state.mode==='c'?'dark':'light';", "state.theme='dark';")
source = source.replace(
    "state.location='workspace';state.modal=null;state.singlePane=false;"
    "state.sidebar=!narrowQuery.matches;if(state.mode==='c')",
    "state.location=state.mode==='a'?'catalog':'workspace';state.modal=null;"
    "state.singlePane=false;state.sidebar=!narrowQuery.matches;if(state.mode==='c')",
)
source, count = re.subn(
    r"^const proto=h=>.*$",
    """const proto=h=>`<span class="proto-icon tile-${h.id%6} ${h.protocol==='SSH'?'':'desktop'}">${icon(h.protocol==='SSH'?'terminal':'desktop')}</span>`;""",
    source, flags=re.M,
)
assert count == 1
ending = "render();\n</script>\n</body>\n</html>\n"
assert source.endswith(ending)
source = source[:-len(ending)] + study + "\n</script>\n</body>\n</html>\n"
(root / "termius.html").write_text(source, encoding="utf-8", newline="\n")
print("Built self-contained termius.html; original A/B/C unchanged.")
