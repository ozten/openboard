from pathlib import Path
import unittest
class Website(unittest.TestCase):
 def test_install_matches_project_and_local_assets_exist(self):
  root=Path(__file__).parent
  self.assertTrue((root/'index.html').exists(), 'Public landing page exists')
  html=(root/'index.html').read_text()
  self.assertIn('curl -fsSL https://raw.githubusercontent.com/ozten/openboard/main/scripts/install.sh | sh',html)
  for asset in ['style.css','site.js','CNAME']:
   self.assertTrue((root/asset).is_file(),asset)
  self.assertEqual((root/'CNAME').read_text().strip(),'openboard.lol')
if __name__=='__main__': unittest.main()
