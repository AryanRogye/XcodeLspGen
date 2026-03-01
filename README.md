# XcodeLspGen
<table>
  <tr>
    <td><b>Scans for Xcode Projects / Workspaces</b></td>
    <td><b>Pick a Scheme</b></td>
    <td><b>Generates <code>buildServer.json</code></b></td>
  </tr>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/5aa4aff0-1ce3-4ba2-bb0b-25eea4d8010f" width="350"></td>
    <td><img src="https://github.com/user-attachments/assets/bac82239-4739-4768-9f5e-b82bb35f93f7" width="350"></td>
    <td><img src="https://github.com/user-attachments/assets/b8ad6dd2-ca74-4f79-a8c2-00976b9d2579" width="350"></td>
  </tr>
</table>

tbh I just realized that I have to make a script to build lsp everytime I make a new project, so I just whipped this up real fast for myself

if u want to build it clone it so:

```bash
git clone https://github.com/AryanRogye/XcodeLspGen.git
cd XcodeLspGen
cargo build --release
```
that puts the executable in `target/release/XcodeLspGen`

then u can move it to wherever its best for you, for me I put it in:
`~/bin/`
